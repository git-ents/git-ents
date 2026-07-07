//! The real [`SpriteLauncher`]: shell out to the `fly` (flyctl) CLI's
//! `machine` commands — the same pattern as `git-effect`'s `docker` and
//! `sprite` backends, and deliberately not an HTTP client against the
//! Machines REST API (dependency policy: no new external dependencies).
//!
//! The `sprite` CLI the checks engine already drives was considered and
//! passed over here: it manages one persistent sandbox per repository and
//! has no image flag, while `exec-sprites`' whole point is one throwaway
//! machine per effect booted from a WS8-baked image. flyctl's `machine
//! run` expresses exactly that.
//!
//! Honesty about coverage: argv assembly ([`run_args`]) and output parsing
//! ([`parse_machine_id`], [`machine_settled`]) are pure and unit-tested;
//! *validating them against a live flyctl* is deploy-only work — flyctl's
//! human-oriented output is unversioned, and nothing in this repository
//! can pin it. Each parsing site carries the caveat.

use std::process::Command;
use std::time::{Duration, Instant};

use git_backend::{Error, Result};

use crate::{MachineSpec, SpriteLauncher};

/// How long [`FlyLauncher::wait`] polls a machine before giving up —
/// matches the effect engine's own 30-minute per-effect timeout, so a
/// wedged machine is abandoned on the same clock as a wedged local run.
const WAIT_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// How often [`FlyLauncher::wait`] polls `fly machine status`.
const POLL: Duration = Duration::from_secs(2);

/// [`SpriteLauncher`] over the `fly` CLI: `fly machine run --rm --detach`
/// to create, `fly machine status` polling to wait. Authentication is
/// flyctl's own (`FLY_API_TOKEN`, or its config file) — this launcher
/// passes nothing secret on any command line.
pub struct FlyLauncher {
    bin: String,
    app: String,
    poll: Duration,
    wait_timeout: Duration,
}

impl FlyLauncher {
    /// A launcher creating machines in the Fly app `app` via the `fly`
    /// binary on `PATH`.
    #[must_use]
    pub fn new(app: impl Into<String>) -> Self {
        Self {
            bin: "fly".to_owned(),
            app: app.into(),
            poll: POLL,
            wait_timeout: WAIT_TIMEOUT,
        }
    }
}

/// `fly machine run`'s argv for `spec` — pure, so the exact invocation is
/// unit-tested without flyctl (the same pattern as
/// `git_effect::docker::run_args`). Flags precede the positional image and
/// command so a command word can never be mistaken for a flag; `--rm`
/// reaps the machine on exit, `--detach` returns once it is created (the
/// executor's `spawn` must not block for completion).
#[must_use]
pub fn run_args(app: &str, spec: &MachineSpec) -> Vec<String> {
    let mut args = vec![
        "machine".to_owned(),
        "run".to_owned(),
        "--app".to_owned(),
        app.to_owned(),
        "--name".to_owned(),
        spec.name.clone(),
        "--rm".to_owned(),
        "--detach".to_owned(),
    ];
    for (key, value) in &spec.env {
        args.push("--env".to_owned());
        args.push(format!("{key}={value}"));
    }
    args.push(spec.image.clone());
    args.push("sh".to_owned());
    args.push("-c".to_owned());
    args.push(spec.command.clone());
    args
}

/// The machine id out of `fly machine run --detach`'s output: the value of
/// its `Machine ID: <id>` line, or, failing that, the first token shaped
/// like a machine id (14 lowercase hex characters). Deploy-only caveat:
/// this matches the output shape current flyctl releases print; a live
/// `fly machine run` is the only authority on whether it still holds.
#[must_use]
pub fn parse_machine_id(output: &str) -> Option<String> {
    for line in output.lines() {
        if let Some(rest) = line.trim().strip_prefix("Machine ID:") {
            let id = rest.trim();
            if !id.is_empty() {
                return Some(id.to_owned());
            }
        }
    }
    output
        .split_whitespace()
        .find(|token| {
            token.len() == 14
                && token
                    .chars()
                    .all(|c| c.is_ascii_digit() || c.is_ascii_lowercase() && c.is_ascii_hexdigit())
        })
        .map(str::to_owned)
}

/// Whether a `fly machine status` output describes a settled machine
/// (stopped or destroyed). Same deploy-only caveat as
/// [`parse_machine_id`].
#[must_use]
pub fn machine_settled(status_output: &str) -> bool {
    let lowered = status_output.to_lowercase();
    lowered.contains("stopped") || lowered.contains("destroyed")
}

impl SpriteLauncher for FlyLauncher {
    fn launch(&self, spec: &MachineSpec) -> Result<String> {
        let output = Command::new(&self.bin)
            .args(run_args(&self.app, spec))
            .output()
            .map_err(|e| {
                Error::Effect(format!(
                    "could not run the fly CLI (is flyctl installed?): {e}"
                ))
            })?;
        if !output.status.success() {
            return Err(Error::Effect(format!(
                "fly machine run failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        parse_machine_id(&stdout).ok_or_else(|| {
            Error::Effect(
                "fly machine run succeeded but no machine id was found in its output".to_owned(),
            )
        })
    }

    fn wait(&self, machine: &str) -> Result<()> {
        let deadline = Instant::now()
            .checked_add(self.wait_timeout)
            .ok_or_else(|| Error::Effect("wait timeout overflowed the clock".to_owned()))?;
        loop {
            let output = Command::new(&self.bin)
                .args(["machine", "status", machine, "--app", &self.app])
                .output()
                .map_err(|e| {
                    Error::Effect(format!(
                        "could not run the fly CLI (is flyctl installed?): {e}"
                    ))
                })?;
            let text = format!(
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            if !output.status.success() {
                // `--rm` reaps the machine on exit, so "not found" after a
                // successful launch means it ran and was already destroyed:
                // settled. (Deploy-only caveat as above.)
                let lowered = text.to_lowercase();
                if lowered.contains("not found") || lowered.contains("could not find") {
                    return Ok(());
                }
                return Err(Error::Effect(format!(
                    "fly machine status failed for {machine}: {}",
                    text.trim()
                )));
            }
            if machine_settled(&text) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(Error::Effect(format!(
                    "machine {machine} did not settle within {:?}",
                    self.wait_timeout
                )));
            }
            std::thread::sleep(self.poll);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, reason = "unit test")]

    use std::collections::BTreeMap;

    use super::*;

    fn spec() -> MachineSpec {
        let mut env = BTreeMap::new();
        env.insert("GIT_ENTS_EFFECT".to_owned(), "test".to_owned());
        MachineSpec {
            name: "effect-test-aaaaaaaaaaaa".to_owned(),
            image: "registry.fly.io/git-ents-effects:baked".to_owned(),
            env,
            command: "cargo test".to_owned(),
        }
    }

    #[test]
    fn run_args_put_flags_before_the_positional_image_and_command() {
        let args = run_args("git-ents-effects", &spec());
        assert_eq!(
            args,
            vec![
                "machine",
                "run",
                "--app",
                "git-ents-effects",
                "--name",
                "effect-test-aaaaaaaaaaaa",
                "--rm",
                "--detach",
                "--env",
                "GIT_ENTS_EFFECT=test",
                "registry.fly.io/git-ents-effects:baked",
                "sh",
                "-c",
                "cargo test",
            ]
        );
    }

    #[test]
    fn parse_machine_id_prefers_the_labeled_line() {
        let output = "Success! A Machine has been successfully launched\n\
                      Machine ID: 148ed599c14189\n\
                      Instance ID: 01HXYZ\n";
        assert_eq!(parse_machine_id(output), Some("148ed599c14189".to_owned()));
    }

    #[test]
    fn parse_machine_id_falls_back_to_an_id_shaped_token() {
        assert_eq!(
            parse_machine_id("launched 148ed599c14189 in yyz"),
            Some("148ed599c14189".to_owned())
        );
        assert_eq!(parse_machine_id("no ids here"), None);
    }

    #[test]
    fn machine_settled_matches_stopped_and_destroyed() {
        assert!(machine_settled("State: stopped"));
        assert!(machine_settled("machine was destroyed"));
        assert!(!machine_settled("State: started"));
    }
}
