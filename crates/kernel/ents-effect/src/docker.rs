//! The Docker [`Executor`] backend (`effect.execution`, `roots.local`):
//! shells out to the `docker` CLI (no docker API crate — the same
//! rationale [`crate::sprite`] uses for the `sprite` CLI), running each
//! effect in a throwaway `--rm` container with the materialized workdir and
//! toolchains bind-mounted in.
//!
//! Ported from `pre-redo`'s `git-effect::docker` module: the readiness
//! probe ([`ensure_docker`]) and the pure argv assembly ([`run_args`]) carry
//! over verbatim (minus the cache-directory bind mount — this design has no
//! effect-level cache, `model.effect-definition`); the rest is rewritten
//! against this phase's [`crate::Executor`] trait.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::error::{Error, Result};
use crate::executor::{
    Executor, RunOutput, SandboxInputs, activate, parse_exit_marker, wrap_exit_marker,
};

/// The minimal base image every effect runs in — no toolchain of its own;
/// everything the command needs comes from the bind-mounted, host-exported
/// toolchains.
pub const IMAGE: &str = "debian:stable-slim";

/// Where the workdir is bind-mounted in the container.
pub const WORKDIR: &str = "/work";

/// Where a toolchain's `bin/` directory is bind-mounted, per toolchain
/// name: `{TOOLCHAINS_DIR}/<name>/bin`.
pub const TOOLCHAINS_DIR: &str = "/toolchains";

/// Confirm `docker` is on `PATH` and the daemon answers, with a clean error
/// — rather than a raw "os error 2" — when it is not. The one place this
/// backend can fail before an effect ever runs.
///
/// # Errors
///
/// [`Error::Spawn`] if `docker` could not be started at all;
/// [`Error::Process`] if it ran but the daemon did not respond.
pub fn ensure_docker() -> Result<()> {
    let status = Command::new("docker")
        .arg("version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| Error::Spawn {
            program: "docker".to_owned(),
            detail: e.to_string(),
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::Process {
            program: "docker".to_owned(),
            detail: "the daemon did not respond to `docker version`; is it running?".to_owned(),
        })
    }
}

/// Assemble `docker run`'s argv for one sandboxed run — pure, so the exact
/// invocation is unit tested without a daemon. The command runs under
/// `sh -c`, stderr folded into stdout so the captured recording is one
/// interleaved stream, wrapped by [`wrap_exit_marker`] so a completed
/// command is distinguishable from a docker-side failure (`docker run`
/// exits 125 when the daemon itself fails — never a result,
/// `effect.result-taxonomy`).
///
/// # Examples
///
/// ```
/// use ents_effect::docker::run_args;
/// use ents_effect::executor::EXIT_MARKER;
/// use std::path::Path;
///
/// let args = run_args(Path::new("/tmp/s/work"), &[], "cargo test");
/// assert!(args.contains(&"/tmp/s/work:/work".to_owned()));
/// assert!(args.contains(&"debian:stable-slim".to_owned()));
/// let script = args.last().expect("has a script");
/// assert!(script.contains("cargo test") && script.contains(EXIT_MARKER));
/// ```
#[must_use]
pub fn run_args(workdir: &Path, toolchains: &[(String, PathBuf)], command: &str) -> Vec<String> {
    let mut args = vec![
        "run".to_owned(),
        "--rm".to_owned(),
        "-v".to_owned(),
        format!("{}:{WORKDIR}", workdir.display()),
    ];
    let mut sandbox_dirs = Vec::with_capacity(toolchains.len());
    for (name, host_dir) in toolchains {
        let sandbox_dir = format!("{TOOLCHAINS_DIR}/{name}/bin");
        args.push("-v".to_owned());
        args.push(format!(
            "{}:{TOOLCHAINS_DIR}/{name}/bin:ro",
            host_dir.display()
        ));
        sandbox_dirs.push((name.clone(), sandbox_dir));
    }
    args.push("-w".to_owned());
    args.push(WORKDIR.to_owned());
    args.push(IMAGE.to_owned());
    args.push("sh".to_owned());
    args.push("-c".to_owned());
    args.push(wrap_exit_marker(&activate(command, &sandbox_dirs)));
    args
}

/// [`Executor`] running each effect in a throwaway local Docker container
/// (`roots.local`).
#[derive(Debug, Clone, Copy, Default)]
pub struct DockerExecutor;

impl Executor for DockerExecutor {
    // @relation(effect.result-taxonomy, scope=function)
    fn run(&self, inputs: &SandboxInputs<'_>) -> Result<RunOutput> {
        ensure_docker()?;
        let args = run_args(inputs.workdir, inputs.toolchains, inputs.command);
        let output = Command::new("docker")
            .args(&args)
            .output()
            .map_err(|e| Error::Spawn {
                program: "docker".to_owned(),
                detail: e.to_string(),
            })?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        // The marker, not docker's exit status, is the completion signal:
        // `docker run` exits 125 for the daemon's own failures (unpullable
        // image, bad mount, daemon dying mid-run), which must surface as an
        // infrastructure error, never as a recorded `fail`
        // (`effect.result-taxonomy`).
        parse_exit_marker(&stdout).ok_or_else(|| {
            Error::Sandbox(format!(
                "docker run did not complete the command (exit {:?}): {}",
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
    // @relation(effect.execution, scope=function, role=Verifies)
    fn run_args_binds_the_workdir() {
        let args = run_args(Path::new("/tmp/s/work"), &[], "cargo test");
        assert_eq!(
            args,
            vec![
                "run".to_owned(),
                "--rm".to_owned(),
                "-v".to_owned(),
                "/tmp/s/work:/work".to_owned(),
                "-w".to_owned(),
                "/work".to_owned(),
                IMAGE.to_owned(),
                "sh".to_owned(),
                "-c".to_owned(),
                wrap_exit_marker("cargo test"),
            ]
        );
    }

    #[rstest]
    // @relation(effect.result-taxonomy, scope=function, role=Verifies)
    fn run_args_script_completes_with_the_exit_marker() {
        let args = run_args(Path::new("/w"), &[], "true");
        let script = args.last().expect("has a script");
        assert!(
            script.contains(crate::executor::EXIT_MARKER),
            "without the marker, a docker-side failure (exit 125) would be \
             indistinguishable from the command failing"
        );
    }

    #[rstest]
    // @relation(effect.execution, effect.toolchains, scope=function, role=Verifies)
    fn run_args_binds_each_toolchain_read_only_and_activates_it() {
        let toolchains = vec![("rust".to_owned(), PathBuf::from("/cache/rust/bin"))];
        let args = run_args(Path::new("/w"), &toolchains, "cargo test");
        assert!(args.contains(&"/cache/rust/bin:/toolchains/rust/bin:ro".to_owned()));
        let last = args.last().expect("has a command");
        assert!(last.contains("export PATH=/toolchains/rust/bin:$PATH; cargo test"));
    }

    #[rstest]
    // @relation(effect.execution, scope=function, role=Verifies)
    fn run_args_uses_the_minimal_base_image() {
        let args = run_args(Path::new("/w"), &[], "true");
        assert_eq!(
            args.get(args.len().saturating_sub(4)).map(String::as_str),
            Some(IMAGE)
        );
    }
}
