//! Docker sandbox backend for local effect execution: shells out to the
//! `docker` CLI via `std::process` (no docker API crate — see
//! [`crate::engine`] for why the Sprite backend does the same with `sprite`),
//! running each effect in a throwaway `--rm` container with the
//! [`crate::local::Sandbox`] materialized on the host bind-mounted in. Unlike
//! the Sprite backend's persistent per-repository sandbox, a container never
//! outlives its run, so nothing here needs an extract-once cache: toolchains
//! are re-materialized on the host per run (cheap — it is a local `git
//! archive`/tree walk, not a network fetch) rather than kept warm across
//! runs.
//!
//! `git effect run` uses this backend by default; `--unsandboxed` skips it
//! for host-direct execution instead (see [`crate::local`]).

use std::path::Path;

/// The minimal base image every effect runs in — no toolchain of its own;
/// everything the command needs comes from the bind-mounted, host-exported
/// toolchains.
pub const IMAGE: &str = "debian:stable-slim";

/// Where the sandbox's work directory is bind-mounted in the container.
pub const WORKDIR: &str = "/work";

/// Where the sandbox's toolchains directory is bind-mounted in the
/// container, read-only — toolchains are extract-once-per-run and never
/// written to by the command.
pub const TOOLCHAINS_DIR: &str = "/toolchains";

/// Where the sandbox's cache directory is bind-mounted in the container,
/// read-write.
pub const CACHE_DIR: &str = "/cache";

/// Confirm `docker` is on `PATH` and the daemon answers, with a clean error
/// (rather than a raw "os error 2") when it is not — the one place this
/// backend can fail before anything else runs.
pub fn ensure_docker() -> Result<(), String> {
    let status = std::process::Command::new("docker")
        .arg("version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| format!("docker is not installed or not on PATH: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("docker is installed but the daemon did not respond (`docker version` failed); is it running?".to_owned())
    }
}

/// Assemble `docker run`'s argv for one effect's `command` against the
/// sandbox's host directories — pure, so the exact invocation is unit tested
/// without a daemon. `command` runs under `sh -c`, stderr folded into stdout
/// so the captured recording is one interleaved stream, matching what the
/// Sprite backend's pty capture already gives a developer.
#[must_use]
pub fn run_args(work: &Path, toolchains: &Path, cache: &Path, command: &str) -> Vec<String> {
    vec![
        "run".to_owned(),
        "--rm".to_owned(),
        "-v".to_owned(),
        format!("{}:{WORKDIR}", work.display()),
        "-v".to_owned(),
        format!("{}:{TOOLCHAINS_DIR}:ro", toolchains.display()),
        "-v".to_owned(),
        format!("{}:{CACHE_DIR}", cache.display()),
        "-w".to_owned(),
        WORKDIR.to_owned(),
        IMAGE.to_owned(),
        "sh".to_owned(),
        "-c".to_owned(),
        format!("{command} 2>&1"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    // @relation(checks.sandbox, role=Verifies)
    #[test]
    fn run_args_binds_work_toolchains_and_cache() {
        let args = run_args(
            Path::new("/tmp/s/work"),
            Path::new("/tmp/s/toolchains"),
            Path::new("/tmp/s/cache"),
            "cargo test",
        );
        assert_eq!(
            args,
            vec![
                "run",
                "--rm",
                "-v",
                "/tmp/s/work:/work",
                "-v",
                "/tmp/s/toolchains:/toolchains:ro",
                "-v",
                "/tmp/s/cache:/cache",
                "-w",
                "/work",
                IMAGE,
                "sh",
                "-c",
                "cargo test 2>&1",
            ]
        );
    }

    // @relation(checks.sandbox, role=Verifies)
    #[test]
    fn run_args_uses_the_minimal_base_image() {
        let args = run_args(Path::new("/w"), Path::new("/t"), Path::new("/c"), "true");
        assert_eq!(args.get(args.len() - 4).map(String::as_str), Some(IMAGE));
    }
}
