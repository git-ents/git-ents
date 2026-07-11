//! The Fly.io Sprite [`Executor`] backend (`effect.execution`,
//! `roots.hosted`): a persistent, hardware-isolated sandbox driven through
//! the `sprite` CLI, one Sprite kept per deployment so a toolchain's
//! extracted bytes survive between runs (`crate::toolchain::materialize`'s
//! host-side cache has a Sprite-side mirror, `sync_dir`'s extract-once
//! check).
//!
//! Ported from `pre-redo`'s `git-effect::engine` Sprite half: the CLI
//! authentication quirk ([`ensure_auth`]), the idempotent-create quirk
//! ([`ensure_sprite`]), and the orphan-process-kill quirk in
//! `unpack_script` all carry over verbatim — these are exactly the
//! "environment is the risk" gotchas the development plan calls out.
//! Rewritten against this phase's design: no `git archive` (this crate
//! never assumes an on-disk `.git`, `arch.no-object-store-trait`) — the
//! workdir and each toolchain are materialized to a host directory first
//! (the same `crate::materialize::checkout` and
//! [`crate::toolchain::materialize`] every backend shares), then `tar`'d
//! from that host directory into the Sprite over `sprite exec`'s stdin;
//! and no PTY/asciicast live-streaming (`effect.adoc` names no such
//! requirement for this phase — deferred to `ents-web`, which owns any
//! live view).

use std::path::Path;
use std::process::{Command, Stdio};

use crate::error::{Error, Result};
use crate::executor::{
    Executor, RunOutput, SandboxInputs, activate, parse_exit_marker, wrap_exit_marker,
};

/// Where the workdir is unpacked inside the Sprite.
pub const WORKDIR: &str = "/work";

/// Where a toolchain's `bin/` is extracted inside the Sprite, one directory
/// per content key (`{TOOLCHAINS_DIR}/<key>/bin`) — never cleared: the
/// Sprite's persistent filesystem is the cache.
pub const TOOLCHAINS_DIR: &str = "/toolchains";

/// The env var the hosted worker passes the `sprite` CLI's auth token
/// through, per [`ensure_auth`].
pub const SPRITES_TOKEN_VAR: &str = "SPRITES_TOKEN";

/// A Sprite name derived from `seed`, kept to the `[a-z0-9-]` a Sprite name
/// allows so the same seed (a deployment id, a repository path) always
/// reuses the same sandbox.
///
/// # Examples
///
/// ```
/// use ents_effect::sprite::sprite_name;
///
/// assert_eq!(sprite_name("git-ents.cloud"), "ents-effect-git-ents-cloud");
/// assert_eq!(sprite_name(""), "ents-effect-sprite");
/// ```
#[must_use]
pub fn sprite_name(seed: &str) -> String {
    let sanitized: String = seed
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = sanitized.trim_matches('-');
    format!(
        "ents-effect-{}",
        if trimmed.is_empty() {
            "sprite"
        } else {
            trimmed
        }
    )
}

/// Configure the `sprite` CLI from [`SPRITES_TOKEN_VAR`]. The CLI persists
/// its credentials to a config file rather than reading the token per
/// call, so without this it reports "no organizations configured" even
/// with the token in the environment. `auth setup` is idempotent, so a
/// caller may run this before every batch of runs to keep the steady state
/// self-healing.
///
/// # Errors
///
/// [`Error::Process`] if [`SPRITES_TOKEN_VAR`] is unset, or the CLI ran and
/// refused it; [`Error::Spawn`] if the CLI could not be started.
pub fn ensure_auth() -> Result<()> {
    let token = std::env::var(SPRITES_TOKEN_VAR).map_err(|_unset| Error::Process {
        program: "sprite".to_owned(),
        detail: format!("{SPRITES_TOKEN_VAR} is not set in the worker's environment"),
    })?;
    let output = Command::new("sprite")
        .args(["auth", "setup", "--token", &token])
        .output()
        .map_err(|e| Error::Spawn {
            program: "sprite".to_owned(),
            detail: e.to_string(),
        })?;
    if output.status.success() {
        Ok(())
    } else {
        Err(Error::Process {
            program: "sprite".to_owned(),
            detail: format!(
                "auth setup failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        })
    }
}

/// Create the Sprite named `name` if it does not already exist.
/// `sprite create` fails when the Sprite is already there — the steady
/// state once the first run has happened — so its failure is tolerated
/// here and surfaces only later if the Sprite turns out unreachable.
///
/// # Errors
///
/// [`Error::Spawn`] if the CLI could not be started.
pub fn ensure_sprite(name: &str) -> Result<()> {
    let _existing = Command::new("sprite")
        .args(["create", "--skip-console", name])
        .output()
        .map_err(|e| Error::Spawn {
            program: "sprite".to_owned(),
            detail: e.to_string(),
        })?;
    Ok(())
}

/// The in-Sprite script [`sync_dir`] runs to replace `dest`'s contents with
/// the tar streamed over stdin, first killing any process still working
/// under `dest`: a worker killed mid-run (a deploy, a restart) leaves its
/// in-Sprite build processes alive, since `sprite exec` only tethers the
/// local CLI process — an orphaned build still writing under `dest` races
/// the wipe, failing `rm -rf` with "Directory not empty".
fn unpack_script(dest: &str) -> String {
    format!(
        "for cwd in /proc/[0-9]*/cwd; do\n\
           case \"$(readlink \"$cwd\" 2>/dev/null)\" in\n\
             {dest}|{dest}/*) kill -9 \"$(basename \"${{cwd%/cwd}}\")\" 2>/dev/null || true ;;\n\
           esac\n\
         done\n\
         rm -rf {dest} && mkdir -p {dest} && tar -x -C {dest}"
    )
}

/// Stream `host_dir`'s contents into the Sprite `name` at `dest`, replacing
/// whatever was there — used both for the workdir (always re-synced: a
/// fresh checkout per run) and, via [`sync_toolchain`], for a toolchain not
/// already cached in-Sprite.
///
/// # Errors
///
/// [`Error::Spawn`] if `tar` or `sprite` could not be started;
/// [`Error::Process`] if either exited nonzero.
fn sync_dir(host_dir: &Path, name: &str, dest: &str) -> Result<()> {
    let mut archive = Command::new("tar")
        .args(["-c", "-C"])
        .arg(host_dir)
        .arg(".")
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| Error::Spawn {
            program: "tar".to_owned(),
            detail: e.to_string(),
        })?;
    let tar_stdout = archive.stdout.take().ok_or_else(|| Error::Process {
        program: "tar".to_owned(),
        detail: "no stdout".to_owned(),
    })?;

    let unpack = Command::new("sprite")
        .args(["exec", "-s", name, "--", "sh", "-c", &unpack_script(dest)])
        .stdin(Stdio::from(tar_stdout))
        .output()
        .map_err(|e| Error::Spawn {
            program: "sprite".to_owned(),
            detail: e.to_string(),
        })?;

    let tar_status = archive.wait().map_err(|e| Error::Process {
        program: "tar".to_owned(),
        detail: e.to_string(),
    })?;
    if !tar_status.success() {
        return Err(Error::Process {
            program: "tar".to_owned(),
            detail: format!("could not archive {}", host_dir.display()),
        });
    }
    if !unpack.status.success() {
        return Err(Error::Process {
            program: "sprite".to_owned(),
            detail: format!(
                "could not sync into {dest}: {}",
                String::from_utf8_lossy(&unpack.stderr).trim()
            ),
        });
    }
    Ok(())
}

/// Extract-once sync of one toolchain's `bin/` directory into the Sprite
/// `name`, at `{TOOLCHAINS_DIR}/<key>/bin` — a directory already present
/// from an earlier run is left alone rather than re-extracted, since the
/// Sprite's persistent filesystem is the cache. `key` is the same content
/// key [`crate::toolchain::materialize`] cached `host_bin`'s parent
/// directory under, so two runs of the same toolchain content sync it at
/// most once.
///
/// # Errors
///
/// See [`sync_dir`].
fn sync_toolchain(host_bin: &Path, name: &str, key: &str) -> Result<String> {
    let sandbox_dir = format!("{TOOLCHAINS_DIR}/{key}/bin");
    let cached = Command::new("sprite")
        .args([
            "exec",
            "-s",
            name,
            "--",
            "sh",
            "-c",
            &format!("[ -d {sandbox_dir} ]"),
        ])
        .status()
        .map_err(|e| Error::Spawn {
            program: "sprite".to_owned(),
            detail: e.to_string(),
        })?;
    if !cached.success() {
        sync_dir(host_bin, name, &sandbox_dir)?;
    }
    Ok(sandbox_dir)
}

/// The content key [`crate::toolchain::materialize`] cached `host_bin`
/// under — `host_bin`'s parent directory name, since `materialize` always
/// returns `<cache_root>/<key>/bin`.
fn content_key(host_bin: &Path) -> Result<String> {
    host_bin
        .parent()
        .and_then(Path::file_name)
        .and_then(std::ffi::OsStr::to_str)
        .map(str::to_owned)
        .ok_or_else(|| Error::Process {
            program: "sprite".to_owned(),
            detail: format!(
                "{} is not a materialize()-shaped toolchain directory",
                host_bin.display()
            ),
        })
}

/// [`Executor`] running each effect in a persistent, hardware-isolated Fly
/// Sprite (`roots.hosted`).
#[derive(Debug, Clone)]
pub struct SpriteExecutor {
    /// The Sprite's name, from [`sprite_name`] or chosen by the
    /// composition root.
    pub name: String,
}

impl SpriteExecutor {
    /// A Sprite executor targeting the Sprite named `name`.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }
}

/// The in-Sprite script for one run: enter the synced workdir, run the
/// activated command wrapped by [`wrap_exit_marker`]. A `cd` failure (the
/// workdir sync silently lost) exits before the marker can print, so it
/// surfaces as infrastructure, not as a recorded `fail` — the same
/// discrimination the marker gives a dying transport
/// (`effect.result-taxonomy`).
fn run_script(activated: &str) -> String {
    format!("cd {WORKDIR} || exit 70\n{}", wrap_exit_marker(activated))
}

impl Executor for SpriteExecutor {
    // @relation(effect.result-taxonomy, scope=function)
    fn run(&self, inputs: &SandboxInputs<'_>) -> Result<RunOutput> {
        ensure_auth()?;
        ensure_sprite(&self.name)?;
        sync_dir(inputs.workdir, &self.name, WORKDIR)?;

        let mut sandbox_dirs: Vec<(String, String)> = Vec::with_capacity(inputs.toolchains.len());
        for (toolchain_name, host_bin) in inputs.toolchains {
            let key = content_key(host_bin)?;
            let sandbox_dir = sync_toolchain(host_bin, &self.name, &key)?;
            sandbox_dirs.push((toolchain_name.clone(), sandbox_dir));
        }

        let script = run_script(&activate(inputs.command, &sandbox_dirs));
        let output = Command::new("sprite")
            .args(["exec", "-s", &self.name, "--", "sh", "-c", &script])
            .output()
            .map_err(|e| Error::Spawn {
                program: "sprite".to_owned(),
                detail: e.to_string(),
            })?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        // The marker, not `sprite exec`'s exit status, is the completion
        // signal: the CLI exits nonzero for its own transport failures too,
        // which must surface as an infrastructure error, never a recorded
        // `fail` (`effect.result-taxonomy`).
        parse_exit_marker(&stdout).ok_or_else(|| {
            Error::Sandbox(format!(
                "sprite exec did not complete the command (exit {:?}): {}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "unit test")]

    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::normal("git-ents.cloud", "ents-effect-git-ents-cloud")]
    #[case::empty("", "ents-effect-sprite")]
    #[case::only_punctuation("///", "ents-effect-sprite")]
    #[case::mixed_case("Repo_Name", "ents-effect-repo-name")]
    // @relation(effect.execution, scope=function, role=Verifies)
    fn sprite_name_sanitizes_to_a_valid_shape(#[case] seed: &str, #[case] expected: &str) {
        assert_eq!(sprite_name(seed), expected);
    }

    #[rstest]
    // @relation(effect.result-taxonomy, scope=function, role=Verifies)
    fn run_script_gates_the_exit_marker_on_a_successful_cd() {
        let script = run_script("cargo test");
        // A failed cd exits before the marker can print, so a lost workdir
        // sync surfaces as infrastructure, never as a recorded fail.
        assert!(script.starts_with("cd /work || exit 70\n"));
        assert!(script.contains(crate::executor::EXIT_MARKER));
        assert!(script.contains("cargo test"));
    }

    #[rstest]
    // @relation(effect.execution, scope=function, role=Verifies)
    fn unpack_script_kills_orphans_before_wiping_the_destination() {
        let script = unpack_script("/work");
        assert!(script.contains("kill -9"));
        assert!(script.contains("rm -rf /work && mkdir -p /work && tar -x -C /work"));
    }

    #[rstest]
    // @relation(effect.toolchains, scope=function, role=Verifies)
    fn content_key_reads_the_materialize_cache_layout() {
        let bin = Path::new("/cache/deadbeef/bin");
        assert_eq!(content_key(bin).expect("valid shape"), "deadbeef");
    }

    #[rstest]
    // @relation(effect.toolchains, scope=function, role=Verifies)
    fn content_key_rejects_a_path_with_no_parent() {
        content_key(Path::new("/")).expect_err("no parent");
    }
}
