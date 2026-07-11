//! The `Executor` seam: one trait, multiple sandbox backends
//! (`effect.execution`).
//!
//! No execution logic is duplicated per backend: [`Executor::run`] is
//! handed a fully materialized workdir and a fully materialized set of
//! toolchain directories (both produced by `crate::materialize::checkout`
//! and [`crate::toolchain::materialize`] — the same code the run loop and
//! `git effect run` share, `effect.local-run`), and does only the
//! backend-specific part: get those bytes into the sandbox, run the
//! command, and report what happened.

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
    /// activated `bin/` (`crate::toolchain::materialize`'s return value),
    /// in the effect's declared order — the order [`activate`] honors when
    /// two toolchains would otherwise collide on `PATH`.
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

#[cfg(test)]
mod tests {
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
}
