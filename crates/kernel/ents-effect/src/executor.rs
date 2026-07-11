//! The `Executor` seam: one trait, multiple sandbox backends
//! (`effect.execution`).
//!
//! No execution logic is duplicated per backend: [`Executor::run`] is
//! handed a fully materialized workdir and a fully materialized set of
//! toolchain directories (the former produced by
//! [`crate::materialize::checkout`], the latter by `ents-kiln`'s
//! toolchain `materialize` — the same code the run loop and `git effect
//! run` share, `effect.local-run`), and does only the backend-specific
//! part: get those bytes into the sandbox, run the command, and report
//! what happened.

use std::path::{Path, PathBuf};

use crate::error::Result;

/// The inputs one sandboxed run needs, already materialized on the host —
/// a backend's only job is to get these into its sandbox and run
/// [`SandboxInputs::command`].
#[derive(Debug, Clone)]
pub struct SandboxInputs<'a> {
    /// The host directory holding the tested commit's checked-out tree.
    pub workdir: &'a Path,
    /// Each declared toolchain's name and the host directory holding its
    /// activated `bin/` (`ents-kiln`'s toolchain `materialize`'s return
    /// value), in the effect's declared order — the order [`activate`]
    /// honors when two toolchains would otherwise collide on `PATH`.
    pub toolchains: &'a [(String, PathBuf)],
    /// The run command, exactly as stored on the effect definition
    /// (`model.effect-definition`).
    pub command: &'a str,
}

/// What a completed run reported. Only `Pass` or `Fail`
/// (`effect.result-taxonomy`: "a completed command's exit status MUST
/// always be recorded as a result"); an infrastructure failure — the
/// sandbox never started — is [`crate::Error`], not a variant here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    /// The command exited zero.
    Pass,
    /// The command exited nonzero.
    Fail,
}

/// The output of one completed sandboxed run.
#[derive(Debug, Clone)]
pub struct RunOutput {
    /// Whether the command passed or failed.
    pub status: RunStatus,
    /// The command's combined stdout/stderr.
    pub log: String,
}

/// One sandbox backend (`effect.execution`): Docker, Sprite, or
/// unsandboxed host-direct — selected only at a composition root
/// (`roots.local`, `roots.hosted`), never by effect data
/// (`effect.deployment-property`).
///
/// # Errors
///
/// [`Executor::run`] returns `Err` only for an infrastructure failure —
/// the sandbox never started, or crashed before the command could report
/// an exit status. A command that ran to completion and merely exited
/// nonzero is `Ok(RunOutput { status: RunStatus::Fail, .. })`, never an
/// `Err` (`effect.result-taxonomy`).
///
/// # Examples
///
/// A minimal executor for tests: runs the command directly on the host, no
/// sandbox at all (this is deliberately *not* [`crate::UnsandboxedExecutor`]
/// — it ignores `toolchains` entirely — so it demonstrates only the trait
/// shape, not the `--unsandboxed` contract).
///
/// ```
/// use ents_effect::{Executor, RunStatus, SandboxInputs};
///
/// struct AlwaysPass;
/// impl Executor for AlwaysPass {
///     fn run(&self, _inputs: &SandboxInputs<'_>) -> ents_effect::Result<ents_effect::RunOutput> {
///         Ok(ents_effect::RunOutput { status: RunStatus::Pass, log: String::new() })
///     }
/// }
///
/// let dir = tempfile::tempdir().expect("tempdir");
/// let inputs = SandboxInputs { workdir: dir.path(), toolchains: &[], command: "true" };
/// let output = AlwaysPass.run(&inputs).expect("infallible");
/// assert_eq!(output.status, RunStatus::Pass);
/// ```
pub trait Executor: Send + Sync {
    /// Run `inputs.command` in this backend's sandbox, materialized from
    /// `inputs.workdir` and `inputs.toolchains`.
    fn run(&self, inputs: &SandboxInputs<'_>) -> Result<RunOutput>;
}

/// Prefix `command` with a `PATH` export activating `dirs`' entries,
/// declared order first (so the first-listed toolchain's `bin` wins on a
/// name collision) — ported from `pre-redo`'s `engine::activate`. `dirs`
/// holds each toolchain's *in-sandbox* path (a backend maps its host
/// [`SandboxInputs::toolchains`] entries to sandbox paths before calling
/// this), so it is a plain string, not a [`Path`].
///
/// # Examples
///
/// ```
/// use ents_effect::executor::activate;
///
/// let dirs = vec![("rust".to_owned(), "/toolchains/rust/bin".to_owned())];
/// assert_eq!(
///     activate("cargo test", &dirs),
///     "export PATH=/toolchains/rust/bin:$PATH; cargo test"
/// );
/// assert_eq!(activate("true", &[]), "true");
/// ```
#[must_use]
pub fn activate(command: &str, dirs: &[(String, String)]) -> String {
    if dirs.is_empty() {
        return command.to_owned();
    }
    let path = dirs
        .iter()
        .map(|(_, dir)| dir.as_str())
        .collect::<Vec<_>>()
        .join(":");
    format!("export PATH={path}:$PATH; {command}")
}

/// The sentinel [`wrap_exit_marker`] appends after the wrapped command, so
/// a CLI-driven backend can tell "the command completed and exited with
/// this status" apart from "the CLI or its transport failed" — the
/// distinction `effect.result-taxonomy` requires: a completed command's
/// exit status is always a result, while an infrastructure failure must
/// never be recorded as one.
pub const EXIT_MARKER: &str = "__ENTS_EFFECT_EXIT=";

/// Wrap `command` so its combined output ends with an [`EXIT_MARKER`] line
/// carrying the command's own exit status, and the wrapping script itself
/// always exits zero once the command has run to completion.
///
/// A backend that shells out to a CLI (`docker run`, `sprite exec`) cannot
/// trust that CLI's exit status to be the command's: `docker run` exits
/// 125 for the daemon's own failures, and a transport can die mid-stream
/// and surface any status at all. With this wrapper, the marker's presence
/// *is* the completion signal — present means the command ran and the
/// marker carries its status ([`parse_exit_marker`]); absent means the
/// sandbox never completed the run, which is [`crate::Error::Sandbox`],
/// never a recorded result (`effect.result-taxonomy`).
///
/// # Examples
///
/// ```
/// use ents_effect::executor::{EXIT_MARKER, wrap_exit_marker};
///
/// let script = wrap_exit_marker("cargo test");
/// assert!(script.contains("cargo test"));
/// assert!(script.contains(EXIT_MARKER));
/// ```
// @relation(effect.result-taxonomy, scope=function)
#[must_use]
pub fn wrap_exit_marker(command: &str) -> String {
    format!("{{\n{command}\n}} 2>&1; printf '\\n{EXIT_MARKER}%s\\n' \"$?\"")
}

/// Read a completed run's status out of `log`, the combined output of a
/// [`wrap_exit_marker`]-wrapped command: the last [`EXIT_MARKER`] line wins
/// (a command may echo the marker itself; the wrapper's own line is always
/// printed after it), and the marker line is stripped from the returned
/// [`RunOutput::log`].
///
/// `None` means the marker never appeared — the sandbox did not complete
/// the run, and the caller must report [`crate::Error::Sandbox`] rather
/// than fabricate a `fail` (`effect.result-taxonomy`).
///
/// # Examples
///
/// ```
/// use ents_effect::executor::{RunStatus, parse_exit_marker};
///
/// let done = parse_exit_marker("hello\n__ENTS_EFFECT_EXIT=0\n").expect("completed");
/// assert_eq!(done.status, RunStatus::Pass);
/// assert_eq!(done.log, "hello");
///
/// // No marker: the run never completed; this is not a result.
/// assert!(parse_exit_marker("transport died").is_none());
/// ```
// @relation(effect.result-taxonomy, scope=function)
#[must_use]
pub fn parse_exit_marker(log: &str) -> Option<RunOutput> {
    let idx = log.rfind(EXIT_MARKER)?;
    let tail = log.get(idx.saturating_add(EXIT_MARKER.len())..)?;
    let code: i32 = tail.lines().next()?.trim().parse().ok()?;
    let cleaned = log.get(..idx).unwrap_or_default().trim_end().to_owned();
    Some(RunOutput {
        status: if code == 0 {
            RunStatus::Pass
        } else {
            RunStatus::Fail
        },
        log: cleaned,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "unit test")]

    use rstest::rstest;

    use super::*;

    #[rstest]
    // @relation(effect.execution, scope=function, role=Verifies)
    fn activate_prefixes_path_in_declared_order() {
        let dirs = vec![
            ("a".to_owned(), "/t/a/bin".to_owned()),
            ("b".to_owned(), "/t/b/bin".to_owned()),
        ];
        assert_eq!(
            activate("run", &dirs),
            "export PATH=/t/a/bin:/t/b/bin:$PATH; run"
        );
    }

    #[rstest]
    // @relation(effect.execution, scope=function, role=Verifies)
    fn activate_is_identity_with_no_toolchains() {
        assert_eq!(activate("run", &[]), "run");
    }

    #[rstest]
    #[case::pass("out\n__ENTS_EFFECT_EXIT=0\n", Some((RunStatus::Pass, "out")))]
    #[case::fail("out\n__ENTS_EFFECT_EXIT=1\n", Some((RunStatus::Fail, "out")))]
    #[case::high_exit("__ENTS_EFFECT_EXIT=127\n", Some((RunStatus::Fail, "")))]
    #[case::no_marker_is_not_a_result("transport died mid-stream", None)]
    #[case::garbled_marker_is_not_a_result("__ENTS_EFFECT_EXIT=oops\n", None)]
    #[case::empty("", None)]
    // @relation(effect.result-taxonomy, scope=function, role=Verifies)
    fn parse_exit_marker_separates_completion_from_infrastructure(
        #[case] log: &str,
        #[case] expected: Option<(RunStatus, &str)>,
    ) {
        let parsed = parse_exit_marker(log);
        match expected {
            Some((status, cleaned)) => {
                let run = parsed.expect("marker present means the run completed");
                assert_eq!(run.status, status);
                assert_eq!(run.log, cleaned);
            }
            None => assert!(parsed.is_none(), "no marker must never become a result"),
        }
    }

    #[rstest]
    // @relation(effect.result-taxonomy, scope=function, role=Verifies)
    fn parse_exit_marker_takes_the_last_marker_when_the_command_echoes_one() {
        let log = "echoing __ENTS_EFFECT_EXIT=1 for fun\n__ENTS_EFFECT_EXIT=0\n";
        let run = parse_exit_marker(log).expect("completed");
        assert_eq!(run.status, RunStatus::Pass);
    }

    #[rstest]
    // @relation(effect.result-taxonomy, scope=function, role=Verifies)
    fn wrap_then_parse_round_trips_through_a_real_shell() {
        for (command, expected) in [("true", RunStatus::Pass), ("false", RunStatus::Fail)] {
            let output = std::process::Command::new("sh")
                .arg("-c")
                .arg(wrap_exit_marker(command))
                .output()
                .expect("sh runs");
            // The wrapper itself exits zero once the command has run to
            // completion, whatever the command's own status was.
            assert!(output.status.success());
            let stdout = String::from_utf8_lossy(&output.stdout);
            let run = parse_exit_marker(&stdout).expect("completed");
            assert_eq!(run.status, expected, "command {command:?}");
        }
    }
}
