//! Recipes for `git ents toolchain import --from <recipe>`.
//!
//! A recipe derives `bin`/`src`/`license`/`version`/`platform` from a local
//! toolchain install a user already has (rustup, ...) instead of requiring
//! them to hand-supply paths and metadata `git-toolchain` itself has no way
//! to discover. This module only locates and describes what's already on
//! disk; it never installs a toolchain.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

/// What a recipe resolved from a local toolchain install, ready to hand to
/// `git_toolchain::import`.
///
/// `_staging`, when present, is a temporary directory `bin` (and `src`, if
/// under it) points into; it is kept alive only so the directory survives
/// until the caller's `import()` call has read it, and is deleted on drop.
pub struct Resolved {
    pub bin: PathBuf,
    pub src: Option<PathBuf>,
    pub license: String,
    pub version: String,
    pub platform: String,
    _staging: Option<TempDir>,
}

/// Resolve `recipe` against `spec` (a recipe-specific selector, e.g. a
/// rustup toolchain name). The only recipe today is `rustup`.
pub fn resolve(recipe: &str, spec: &str) -> Result<Resolved, String> {
    match recipe {
        "rustup" => rustup(spec),
        other => Err(format!(
            "unknown toolchain recipe {other:?} (known: rustup)"
        )),
    }
}

/// Resolve a rustup-managed toolchain named `spec` (e.g. `stable`,
/// `1.75.0`, `nightly`) via `rustc +<spec> -vV`, which reports the
/// toolchain's own `release` (its version) and `host` (its target platform)
/// without needing rustup's own metadata format.
///
/// A rustup sysroot's `bin/*` binaries are linked against `lib/*.dylib` (or
/// `.so`) via an rpath relative to `bin`'s own parent (`@loader_path/../lib`
/// on macOS, `$ORIGIN/../lib` on Linux) — but `git-toolchain` activates a
/// toolchain by extracting `bin` as-is and putting *that* directory straight
/// on `PATH`, with no sibling `lib` beside it. Passing `sysroot/bin` alone
/// therefore produces a `rustc` that can neither load its own shared runtime
/// nor find its own standard library to link against. This function instead
/// stages a self-contained directory: `sysroot/bin`'s executables copied to
/// its top level (so `PATH` still finds them directly) plus the whole of
/// `sysroot/lib` copied under a `lib/` subdirectory inside it, with each
/// binary's rpath rewritten from `../lib` to `lib` so it resolves relative
/// to wherever the toolchain ends up extracted, not relative to `bin`'s
/// original location. `src` is `<sysroot>/lib/rustlib/src/rust`, unstaged,
/// when the `rust-src` component is installed, else omitted. Rust's own
/// toolchain is dual-licensed `MIT OR Apache-2.0`.
fn rustup(spec: &str) -> Result<Resolved, String> {
    let toolchain_arg = format!("+{spec}");
    let sysroot = rustc(&toolchain_arg, &["--print", "sysroot"])?;
    let sysroot = PathBuf::from(sysroot.trim());

    let verbose = rustc(&toolchain_arg, &["-vV"])?;
    let version = verbose_field(&verbose, "release")
        .ok_or_else(|| format!("rustc +{spec} -vV did not report a release"))?;
    let platform = verbose_field(&verbose, "host")
        .ok_or_else(|| format!("rustc +{spec} -vV did not report a host"))?;

    let staging = tempfile::tempdir()
        .map_err(|error| format!("could not create a staging directory: {error}"))?;
    stage_bin(&sysroot.join("bin"), &sysroot.join("lib"), staging.path())?;

    let src = sysroot.join("lib/rustlib/src/rust");
    let src = src.is_dir().then_some(src);

    Ok(Resolved {
        bin: staging.path().to_owned(),
        src,
        license: "MIT OR Apache-2.0".to_owned(),
        version,
        platform,
        _staging: Some(staging),
    })
}

/// Copy `bin_src`'s executables flat into `staging`, relink each one's rpath
/// from `bin`-relative (`../lib`) to `staging`-relative (`lib`), then copy
/// the whole of `lib_src` under `staging/lib`.
fn stage_bin(bin_src: &Path, lib_src: &Path, staging: &Path) -> Result<(), String> {
    for entry in fs::read_dir(bin_src)
        .map_err(|error| format!("could not read {}: {error}", bin_src.display()))?
    {
        let entry =
            entry.map_err(|error| format!("could not read {}: {error}", bin_src.display()))?;
        let dest = staging.join(entry.file_name());
        fs::copy(entry.path(), &dest)
            .map_err(|error| format!("could not copy {}: {error}", entry.path().display()))?;
        let mut perms = fs::metadata(&dest)
            .map_err(|error| format!("could not read {}: {error}", dest.display()))?
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&dest, perms)
            .map_err(|error| format!("could not set permissions on {}: {error}", dest.display()))?;
        relink_rpath(&dest)?;
    }
    copy_dir_all(lib_src, &staging.join("lib"))
}

/// Rewrite a copied rustup binary's rpath so it finds its runtime libraries
/// relative to wherever it ends up on disk (`staging/lib`, later
/// `<extracted-toolchain>/lib`) rather than relative to its original
/// `sysroot/bin` location. Failures are ignored: not every entry under
/// `bin/` is a binary carrying this rpath (`rust-gdb`, `rust-lldb`, ... are
/// shell scripts), and a tool that never dynamically links against `lib/`
/// needs no relinking.
fn relink_rpath(path: &Path) -> Result<(), String> {
    match std::env::consts::OS {
        "macos" => {
            drop(
                Command::new("install_name_tool")
                    .args(["-rpath", "@loader_path/../lib", "@loader_path/lib"])
                    .arg(path)
                    .output(),
            );
            Ok(())
        }
        "linux" => {
            let output = Command::new("patchelf")
                .args(["--set-rpath", "$ORIGIN/lib"])
                .arg(path)
                .output()
                .map_err(|error| {
                    format!(
                        "could not run patchelf, required to relocate a rustup toolchain's \
                         runtime library path on linux: {error}"
                    )
                })?;
            let _ = output;
            Ok(())
        }
        other => Err(format!(
            "the rustup recipe does not know how to relocate a toolchain's runtime library \
             path on {other}"
        )),
    }
}

/// Recursively copy `src` to `dst`, preserving permissions and symlinks —
/// `std::fs` has no directory-copy of its own.
fn copy_dir_all(src: &Path, dst: &Path) -> Result<(), String> {
    fs::create_dir_all(dst)
        .map_err(|error| format!("could not create {}: {error}", dst.display()))?;
    for entry in
        fs::read_dir(src).map_err(|error| format!("could not read {}: {error}", src.display()))?
    {
        let entry = entry.map_err(|error| format!("could not read {}: {error}", src.display()))?;
        let file_type = entry
            .file_type()
            .map_err(|error| format!("could not read {}: {error}", entry.path().display()))?;
        let dest_path = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_all(&entry.path(), &dest_path)?;
        } else if file_type.is_symlink() {
            let target = fs::read_link(entry.path()).map_err(|error| {
                format!("could not read symlink {}: {error}", entry.path().display())
            })?;
            std::os::unix::fs::symlink(&target, &dest_path)
                .map_err(|error| format!("could not symlink {}: {error}", dest_path.display()))?;
        } else {
            fs::copy(entry.path(), &dest_path)
                .map_err(|error| format!("could not copy {}: {error}", entry.path().display()))?;
            let perms = fs::metadata(entry.path())
                .map_err(|error| format!("could not read {}: {error}", entry.path().display()))?
                .permissions();
            fs::set_permissions(&dest_path, perms).map_err(|error| {
                format!(
                    "could not set permissions on {}: {error}",
                    dest_path.display()
                )
            })?;
        }
    }
    Ok(())
}

/// Run `rustc <toolchain_arg> <args>` and return its stdout, so a missing
/// toolchain or missing `rustc`/`rustup` shim surfaces as a plain error
/// rather than a panic.
fn rustc(toolchain_arg: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new("rustc")
        .arg(toolchain_arg)
        .args(args)
        .output()
        .map_err(|error| format!("could not run rustc: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "rustc {toolchain_arg} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    String::from_utf8(output.stdout).map_err(|_error| "rustc output was not valid UTF-8".to_owned())
}

/// Extract `<name>: <value>` from `rustc -vV`'s line-oriented output.
fn verbose_field(output: &str, name: &str) -> Option<String> {
    output
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{name}: ")))
        .map(str::to_owned)
}
