//! Recipes for `git ents toolchain import --from <recipe>`.
//!
//! A recipe derives `bin`/`src`/`license`/`version`/`platform` from a local
//! toolchain install a user already has (rustup, ...) instead of requiring
//! them to hand-supply paths and metadata `git-toolchain` itself has no way
//! to discover. This module only locates and describes what's already on
//! disk; it never installs a toolchain.

use std::path::PathBuf;
use std::process::Command;

/// What a recipe resolved from a local toolchain install, ready to hand to
/// `git_toolchain::import`.
pub struct Resolved {
    pub bin: PathBuf,
    pub src: Option<PathBuf>,
    pub license: String,
    pub version: String,
    pub platform: String,
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
/// without needing rustup's own metadata format. `bin` is `<sysroot>/bin`;
/// `src` is `<sysroot>/lib/rustlib/src/rust` when the `rust-src` component
/// is installed, else omitted. Rust's own toolchain is dual-licensed
/// `MIT OR Apache-2.0`.
fn rustup(spec: &str) -> Result<Resolved, String> {
    let toolchain_arg = format!("+{spec}");
    let sysroot = rustc(&toolchain_arg, &["--print", "sysroot"])?;
    let sysroot = PathBuf::from(sysroot.trim());

    let verbose = rustc(&toolchain_arg, &["-vV"])?;
    let version = verbose_field(&verbose, "release")
        .ok_or_else(|| format!("rustc +{spec} -vV did not report a release"))?;
    let platform = verbose_field(&verbose, "host")
        .ok_or_else(|| format!("rustc +{spec} -vV did not report a host"))?;

    let bin = sysroot.join("bin");
    let src = sysroot.join("lib/rustlib/src/rust");
    let src = src.is_dir().then_some(src);

    Ok(Resolved {
        bin,
        src,
        license: "MIT OR Apache-2.0".to_owned(),
        version,
        platform,
    })
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
