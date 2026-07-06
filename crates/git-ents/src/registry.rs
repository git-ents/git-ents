//! Recipes for `git ents toolchain import --from <recipe>`.
//!
//! A recipe derives `bin`/`src`/`license`/`version`/`platform` from a local
//! toolchain install a user already has (rustup, ...) instead of requiring
//! them to hand-supply paths and metadata `git-toolchain` itself has no way
//! to discover. This module only locates and describes what's already on
//! disk (or, for `bin`, what a distributor already hosts); it never installs
//! a toolchain.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use facet::Facet;
use git_toolchain::Component;
use tempfile::TempDir;

/// How a recipe resolved `bin`: either a local directory to import as-is (the
/// embedded path, `--embed`), or a list of externally-hosted components to
/// record as a [`git_toolchain::Bin::Downloaded`] manifest instead of
/// importing local bytes.
#[derive(Clone)]
pub enum Bin {
    Dir(PathBuf),
    Components(Vec<Component>),
}

/// What a recipe resolved from a local toolchain install, ready to hand to
/// `git_toolchain::import`/`import_downloaded`. A `None` metadata field is
/// one the recipe cannot know (the `url` recipe knows nothing about what an
/// arbitrary archive contains); the CLI's own flag-else-prompt chain covers
/// it.
///
/// `_staging`, when `bin` is [`Bin::Dir`] pointing into a temporary
/// directory, is kept alive only so the directory survives until the
/// caller's `import()` call has read it, and is deleted on drop.
pub struct Resolved {
    pub bin: Bin,
    pub src: Option<PathBuf>,
    pub license: Option<String>,
    pub version: Option<String>,
    pub platform: Option<String>,
    _staging: Option<TempDir>,
}

/// How [`resolve`] should resolve, beyond the recipe's own `spec` selector.
#[derive(Default)]
pub struct RecipeOptions {
    /// Import the recipe's actual local `bin` bytes instead of recording
    /// hosted, hash-pinned archives. Incompatible with `platform`.
    pub embed: bool,
    /// Resolve for this target triple instead of the local machine's — the
    /// recipe then never touches local binaries, only the distributor's
    /// hosted metadata and archives, so a toolchain for the effect worker's
    /// sandbox can be pinned from any machine.
    pub platform: Option<String>,
    /// `url` recipe only: leading path segments to strip at extraction
    /// (default 1, a flat `<pkg>-<version>/…` release tarball).
    pub strip: Option<u8>,
    /// `url` recipe only: subdirectory of the toolchain to extract into
    /// (default `bin`, so a flat archive's payload lands on `PATH`).
    pub dest: Option<String>,
}

/// A recipe `git ents toolchain import --from` accepts, described richly
/// enough to render on its own via `facet_pretty` (see `git ents toolchain
/// recipes`) rather than as a bare name.
#[derive(Facet)]
pub struct RecipeInfo {
    /// The name passed to `--from`.
    pub name: &'static str,
    /// What `--from-spec` selects for this recipe, e.g. a rustup channel or
    /// version name.
    pub spec: &'static str,
    /// What this recipe does, in one line.
    pub summary: &'static str,
}

/// Every recipe `resolve` knows, for `git ents toolchain recipes` and
/// `resolve`'s own error message — a plain list rather than a trait registry,
/// since each recipe is one function with its own selector semantics, not a
/// uniform interface worth abstracting over for a list of one.
pub const RECIPES: &[RecipeInfo] = &[
    RecipeInfo {
        name: "rustup",
        spec: "a channel or version, e.g. stable, nightly, 1.75.0",
        summary: "Resolves a rustup-managed toolchain via `rustc +<spec> -vV`; \
                  by default points at rust-lang's own hosted, hash-pinned \
                  component archives instead of importing local bytes.",
    },
    RecipeInfo {
        name: "sccache",
        spec: "a mozilla/sccache release tag, e.g. v0.8.2, or empty for latest",
        summary: "Resolves a prebuilt sccache release from GitHub; with \
                  --platform it records the release archive as a hash pin \
                  computed at import time (trust on first use — GitHub \
                  publishes no hash manifest), otherwise it downloads this \
                  machine's archive and imports the binary directly.",
    },
    RecipeInfo {
        name: "url",
        spec: "an archive URL, e.g. https://ziglang.org/download/.../zig-x86_64-linux-0.15.2.tar.xz",
        summary: "Pins any hosted archive as a downloaded toolchain: fetches \
                  it once at import time only to compute its sha256 (trust \
                  on first use), records url+hash+layout (--strip, --dest), \
                  and lets the sandbox fetch the bytes itself. Version, \
                  platform, and license must be supplied explicitly.",
    },
];

/// Resolve `recipe` against `spec` (a recipe-specific selector, e.g. a
/// rustup toolchain name). See [`RECIPES`] for what's known.
///
/// `opts.embed` forces the old behavior of staging and importing `bin`'s
/// actual bytes; by default the recipe instead points at its distributor's
/// own hosted, hash-verified archives (see [`Bin::Components`]), sparing the
/// repository the toolchain's own bytes. `opts.platform` resolves for a
/// foreign target without touching local binaries at all, and is therefore
/// rejected together with `embed`.
///
/// ## Requirements
///
/// @relation(cli.toolchains)
pub fn resolve(recipe: &str, spec: &str, opts: &RecipeOptions) -> Result<Resolved, String> {
    if opts.embed && opts.platform.is_some() {
        return Err(
            "--embed imports this machine's bytes; it cannot target another platform".to_owned(),
        );
    }
    if (opts.strip.is_some() || opts.dest.is_some()) && recipe != "url" {
        return Err(format!(
            "--strip/--dest are layout hints for the url recipe; {recipe} records its own layout"
        ));
    }
    match recipe {
        "rustup" => rustup(spec, opts),
        "sccache" => sccache(spec, opts.platform.as_deref()),
        "url" => url_archive(spec, opts),
        other => Err(format!(
            "unknown toolchain recipe {other:?} (known: {})",
            RECIPES
                .iter()
                .map(|recipe| recipe.name)
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// `<recipe> <spec>`, recorded as [`git_toolchain::Toolchain::recipe`] — the
/// provenance a `--from` import leaves behind, distinct from `Resolved`
/// itself since only the recipe name and selector (not the resolved bytes)
/// are worth keeping once the import is written.
pub fn describe(recipe: &str, spec: &str) -> String {
    format!("{recipe} {spec}")
}

/// Resolve a rustup-managed toolchain named `spec` (e.g. `stable`,
/// `1.75.0`, `nightly`) via `rustc +<spec> -vV`, which reports the
/// toolchain's own `release` (its version) and `host` (its target platform)
/// without needing rustup's own metadata format.
///
/// By default `bin` is resolved as [`Bin::Components`]: the `rustc`,
/// `cargo`, and `rust-std` entries of rust-lang's own published channel
/// manifest for `version` (or the `nightly` channel manifest, which has no
/// stable per-version name, when `version` is a nightly), each already
/// hash-pinned by rust-lang. These are real rustup-installer archives: every
/// one unpacks to `<package>-<version>-<target>/<component>/...`, so
/// `git_toolchain::export`'s extraction strips exactly that two-segment
/// prefix rather than needing this recipe to relocate anything.
///
/// With `embed`, `bin` is resolved the old way instead: a rustup sysroot's
/// `bin/*` binaries are linked against `lib/*.dylib` (or `.so`) via an rpath
/// relative to `bin`'s own parent (`@loader_path/../lib` on macOS,
/// `$ORIGIN/../lib` on Linux) — but `git-toolchain` activates an embedded
/// toolchain by extracting `bin` as-is and putting *that* directory straight
/// on `PATH`, with no sibling `lib` beside it. Passing `sysroot/bin` alone
/// therefore produces a `rustc` that can neither load its own shared runtime
/// nor find its own standard library to link against. This function instead
/// stages a self-contained directory: `sysroot/bin`'s executables copied to
/// its top level (so `PATH` still finds them directly) plus the whole of
/// `sysroot/lib` copied under a `lib/` subdirectory inside it, with each
/// binary's rpath rewritten from `../lib` to `lib` so it resolves relative
/// to wherever the toolchain ends up extracted, not relative to `bin`'s
/// original location.
///
/// `src` is `<sysroot>/lib/rustlib/src/rust`, unstaged, when the `rust-src`
/// component is installed, else omitted, regardless of `embed`. Rust's own
/// toolchain is dual-licensed `MIT OR Apache-2.0`.
///
/// With `opts.platform`, no local toolchain is consulted at all: the channel
/// manifest for `spec` (`stable`, `nightly`, a version) is the sole source —
/// it names its own version (`[pkg.rustc].version`) and hosts hash-pinned
/// archives for every target, so a toolchain for a foreign platform (the
/// effect worker's sandbox) can be pinned from any machine. `src` is omitted
/// there: there is no local sysroot to point at.
///
/// ## Requirements
///
/// @relation(cli.toolchains)
fn rustup(spec: &str, opts: &RecipeOptions) -> Result<Resolved, String> {
    if let Some(platform) = &opts.platform {
        let manifest = fetch_manifest(spec)?;
        let version = manifest_version(&manifest)
            .ok_or_else(|| format!("channel-rust-{spec}.toml has no [pkg.rustc].version"))?;
        return Ok(Resolved {
            bin: Bin::Components(manifest_components(&manifest, platform)?),
            src: None,
            license: Some("MIT OR Apache-2.0".to_owned()),
            version: Some(version),
            platform: Some(platform.clone()),
            _staging: None,
        });
    }

    let toolchain_arg = format!("+{spec}");
    let sysroot = rustc(&toolchain_arg, &["--print", "sysroot"])?;
    let sysroot = PathBuf::from(sysroot.trim());

    let verbose = rustc(&toolchain_arg, &["-vV"])?;
    let version = verbose_field(&verbose, "release")
        .ok_or_else(|| format!("rustc +{spec} -vV did not report a release"))?;
    let platform = verbose_field(&verbose, "host")
        .ok_or_else(|| format!("rustc +{spec} -vV did not report a host"))?;

    let src = sysroot.join("lib/rustlib/src/rust");
    let src = src.is_dir().then_some(src);

    let (bin, staging) = if opts.embed {
        let staging = tempfile::tempdir()
            .map_err(|error| format!("could not create a staging directory: {error}"))?;
        stage_bin(&sysroot.join("bin"), &sysroot.join("lib"), staging.path())?;
        (Bin::Dir(staging.path().to_owned()), Some(staging))
    } else {
        let manifest = fetch_manifest(&channel_for(&version))?;
        (
            Bin::Components(manifest_components(&manifest, &platform)?),
            None,
        )
    };

    Ok(Resolved {
        bin,
        src,
        license: Some("MIT OR Apache-2.0".to_owned()),
        version: Some(version),
        platform: Some(platform),
        _staging: staging,
    })
}

/// Resolve a prebuilt `sccache` release named `spec` (a GitHub release tag,
/// e.g. `v0.8.2`), or the latest release when `spec` is empty, from
/// `mozilla/sccache`'s GitHub releases — the archive matching this machine's
/// own OS/architecture, the same "what's already usable here" convention
/// [`rustup`] follows via its local `rustc`.
///
/// Unlike `rustup`, GitHub publishes no manifest of hashes alongside a
/// release to pin a hosted download against. Without a `platform` override
/// this recipe therefore imports this machine's archive's binary directly
/// (`Bin::Dir`). With `platform`, it instead records the release archive as
/// a downloaded component whose sha256 it computes itself, once, at import
/// time — trust on first use: the trust decision is taken exactly here, is
/// audited via the recipe string on the document and the ref's commit
/// history, and every later fetch (local export, the sandbox) verifies
/// against the pinned hash. The archive is flat
/// (`sccache-<tag>-<target>/sccache`), hence `strip: 1, dest: "bin"`.
///
/// ## Requirements
///
/// @relation(cli.toolchains)
fn sccache(spec: &str, platform: Option<&str>) -> Result<Resolved, String> {
    let tag = if spec.is_empty() {
        latest_sccache_tag()?
    } else {
        spec.to_owned()
    };
    let version = tag.strip_prefix('v').unwrap_or(&tag).to_owned();
    let target = match platform {
        Some(platform) => platform.to_owned(),
        None => sccache_target()?.to_owned(),
    };
    let url = format!(
        "https://github.com/mozilla/sccache/releases/download/{tag}/sccache-{tag}-{target}.tar.gz"
    );
    let bytes = crate::http_get_bytes(&url)?;

    if platform.is_some() {
        let sha256 = git_toolchain::sha256_hex(&bytes)
            .map_err(|error| format!("could not hash: {error}"))?;
        return Ok(Resolved {
            bin: Bin::Components(vec![Component {
                url,
                sha256,
                strip: 1,
                dest: "bin".to_owned(),
            }]),
            src: None,
            license: Some("MPL-2.0".to_owned()),
            version: Some(version),
            platform: Some(target),
            _staging: None,
        });
    }

    let staging = tempfile::tempdir()
        .map_err(|error| format!("could not create a staging directory: {error}"))?;
    stage_sccache(&bytes, &tag, &target, staging.path())?;

    Ok(Resolved {
        bin: Bin::Dir(staging.path().to_owned()),
        src: None,
        license: Some("MPL-2.0".to_owned()),
        version: Some(version),
        platform: Some(target),
        _staging: Some(staging),
    })
}

/// Pin any hosted archive as a one-component downloaded toolchain —
/// `http_archive`, in Bazel terms. `spec` is the archive's URL; it is
/// fetched once, here, only to compute the sha256 every later verification
/// pins against (trust on first use, audited exactly like [`sccache`]'s
/// pin). The layout hints come from `--strip`/`--dest` (default: a flat
/// `<pkg>-<version>/…` tarball whose payload belongs on `PATH`). This recipe
/// knows nothing about what the archive contains, so version, platform, and
/// license all stay `None` for the caller to supply.
///
/// ## Requirements
///
/// @relation(cli.toolchains)
fn url_archive(spec: &str, opts: &RecipeOptions) -> Result<Resolved, String> {
    if spec.is_empty() {
        return Err("the url recipe needs --spec <archive-url>".to_owned());
    }
    let bytes = crate::http_get_bytes(spec)?;
    let sha256 =
        git_toolchain::sha256_hex(&bytes).map_err(|error| format!("could not hash: {error}"))?;
    Ok(Resolved {
        bin: Bin::Components(vec![Component {
            url: spec.to_owned(),
            sha256,
            strip: opts.strip.unwrap_or(1),
            dest: opts.dest.clone().unwrap_or_else(|| "bin".to_owned()),
        }]),
        src: None,
        license: None,
        version: None,
        platform: None,
        _staging: None,
    })
}

/// The latest `mozilla/sccache` release's tag name, from GitHub's "latest
/// release" API.
fn latest_sccache_tag() -> Result<String, String> {
    let body = crate::http_get("https://api.github.com/repos/mozilla/sccache/releases/latest")?;
    json_string_field(&body, "tag_name")
        .ok_or_else(|| "GitHub's latest sccache release response carried no tag_name".to_owned())
}

/// Extract `"<key>": "value"` from a flat JSON response — a hand-rolled
/// reader for the one field this recipe needs from GitHub's release API,
/// rather than a full JSON parser for a format this is the only caller of.
fn json_string_field(body: &str, key: &str) -> Option<String> {
    let prefix = format!("\"{key}\": \"");
    let rest = body.split_once(&prefix)?.1;
    let end = rest.find('"')?;
    rest.get(..end).map(str::to_owned)
}

/// This machine's OS/architecture as an `mozilla/sccache` release asset
/// name's platform segment (e.g. `x86_64-unknown-linux-musl`).
fn sccache_target() -> Result<&'static str, String> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-musl"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        (os, arch) => Err(format!(
            "the sccache recipe does not know a release asset for {os}/{arch}"
        )),
    }
}

/// Unpack `bytes` (an `sccache-<tag>-<target>.tar.gz` release archive) and
/// copy its `sccache` binary to the top level of `staging`, executable —
/// where `git-toolchain`'s `Bin::Embedded` extraction expects an embedded
/// toolchain's binaries to live.
fn stage_sccache(bytes: &[u8], tag: &str, target: &str, staging: &Path) -> Result<(), String> {
    let scratch =
        tempfile::tempdir().map_err(|error| format!("could not create a temp dir: {error}"))?;
    let archive_path = scratch.path().join("sccache.tar.gz");
    fs::write(&archive_path, bytes)
        .map_err(|error| format!("could not write the downloaded archive: {error}"))?;
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(&archive_path)
        .arg("-C")
        .arg(scratch.path())
        .status()
        .map_err(|error| format!("could not run tar: {error}"))?;
    if !status.success() {
        return Err("could not extract the sccache archive".to_owned());
    }
    let binary = scratch
        .path()
        .join(format!("sccache-{tag}-{target}"))
        .join("sccache");
    let dest = staging.join("sccache");
    fs::copy(&binary, &dest)
        .map_err(|error| format!("could not copy {}: {error}", binary.display()))?;
    let mut perms = fs::metadata(&dest)
        .map_err(|error| format!("could not read {}: {error}", dest.display()))?
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&dest, perms)
        .map_err(|error| format!("could not set permissions on {}: {error}", dest.display()))
}

/// The manifest name for a version rustc reported: nightly builds collapse
/// to the shared `nightly` channel, since rust-lang publishes no stable
/// per-version manifest name for them.
fn channel_for(version: &str) -> String {
    if version.contains("nightly") {
        "nightly".to_owned()
    } else {
        version.to_owned()
    }
}

/// Fetch rust-lang's channel manifest for `channel` (`stable`, `nightly`, or
/// a version) — the one authoritative document naming the channel's version
/// and every target's hash-pinned component archives.
fn fetch_manifest(channel: &str) -> Result<String, String> {
    let url = format!("https://static.rust-lang.org/dist/channel-rust-{channel}.toml");
    crate::http_get(&url)
}

/// The channel's own version, from `[pkg.rustc] version = "1.88.0 (hash
/// date)"` — the first whitespace-separated token, valid semver for stable
/// (`1.88.0`) and nightly (`1.90.0-nightly`) alike.
fn manifest_version(manifest: &str) -> Option<String> {
    let raw = manifest_field(manifest, "pkg.rustc", "version")?;
    raw.split_whitespace().next().map(str::to_owned)
}

/// The three components of rust-lang's channel manifest that together make
/// a working toolchain (compiler, cargo, and the target's standard library),
/// resolved for `target`. Every rust-lang dist archive unpacks to
/// `<package>-<version>-<target>/<component>/…`, hence `strip: 2` with no
/// `dest` — the payload carries its own `bin/`/`lib/` top level.
fn manifest_components(manifest: &str, target: &str) -> Result<Vec<Component>, String> {
    ["rustc", "cargo", "rust-std"]
        .into_iter()
        .map(|package| {
            let section = format!("pkg.{package}.target.{target}");
            let component_url = manifest_field(manifest, &section, "url")
                .ok_or_else(|| format!("the channel manifest has no [{section}].url"))?;
            let sha256 = manifest_field(manifest, &section, "hash")
                .ok_or_else(|| format!("the channel manifest has no [{section}].hash"))?;
            Ok(Component {
                url: component_url,
                sha256,
                strip: 2,
                dest: String::new(),
            })
        })
        .collect()
}

/// Extract `<key> = "value"` from `manifest`'s `[section]` table.
///
/// A hand-rolled reader for the one shape this recipe needs from rust-lang's
/// channel manifest TOML (a flat `key = "value"` line under a `[section]`
/// header), rather than a full TOML parser for a format this is the only
/// caller of.
fn manifest_field(manifest: &str, section: &str, key: &str) -> Option<String> {
    let prefix = format!("{key} = \"");
    let mut in_section = false;
    for line in manifest.lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            in_section = name == section;
            continue;
        }
        if in_section && let Some(rest) = line.strip_prefix(&prefix) {
            return rest.strip_suffix('"').map(str::to_owned);
        }
    }
    None
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
