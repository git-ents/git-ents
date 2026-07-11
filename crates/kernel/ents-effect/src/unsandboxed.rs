//! Host-direct execution, with no sandbox at all (`effect.execution`:
//! "Host-direct execution... MUST require an explicit `--unsandboxed` flag
//! and MUST be available only locally, never on canonical hosted
//! infrastructure").
//!
//! This module only provides the [`Executor`] implementation; enforcing
//! that it is reachable solely behind an explicit flag, and never wired at
//! a hosted composition root, is `roots.local`'s and the future CLI's job
//! (`roots.config-isolation`: selection happens at the root, never inside
//! a library).

use std::process::Command;

use crate::error::{Error, Result};
use crate::executor::{Executor, RunOutput, RunStatus, SandboxInputs, activate};

/// [`Executor`] running a command directly on the host, with the tested
/// tree's checkout as its working directory and every declared toolchain's
/// `bin/` activated on `PATH` — no isolation whatsoever.
#[derive(Debug, Clone, Copy, Default)]
pub struct UnsandboxedExecutor;

impl Executor for UnsandboxedExecutor {
    fn run(&self, inputs: &SandboxInputs<'_>) -> Result<RunOutput> {
        let dirs: Vec<(String, String)> = inputs
            .toolchains
            .iter()
            .map(|(name, dir)| (name.clone(), dir.display().to_string()))
            .collect();
        let output = Command::new("sh")
            .arg("-c")
            .arg(activate(inputs.command, &dirs))
            .current_dir(inputs.workdir)
            .output()
            .map_err(|e| Error::Spawn {
                program: "sh".to_owned(),
                detail: e.to_string(),
            })?;
        let mut log = String::from_utf8_lossy(&output.stdout).into_owned();
        log.push_str(&String::from_utf8_lossy(&output.stderr));
        let status = if output.status.success() {
            RunStatus::Pass
        } else {
            RunStatus::Fail
        };
        Ok(RunOutput { status, log })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "unit test")]

    use std::path::PathBuf;

    use rstest::rstest;

    use super::*;

    #[rstest]
    // @relation(effect.execution, scope=function, role=Verifies)
    fn unsandboxed_reports_pass_on_exit_zero() {
        let dir = tempfile::tempdir().expect("tempdir");
        let inputs = SandboxInputs {
            workdir: dir.path(),
            toolchains: &[],
            command: "true",
        };
        let output = UnsandboxedExecutor.run(&inputs).expect("runs");
        assert_eq!(output.status, RunStatus::Pass);
    }

    #[rstest]
    // @relation(effect.execution, scope=function, role=Verifies)
    fn unsandboxed_reports_fail_on_nonzero_exit_never_as_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let inputs = SandboxInputs {
            workdir: dir.path(),
            toolchains: &[],
            command: "false",
        };
        let output = UnsandboxedExecutor
            .run(&inputs)
            .expect("a completed run is never Err");
        assert_eq!(output.status, RunStatus::Fail);
    }

    #[rstest]
    // @relation(effect.execution, effect.toolchains, scope=function, role=Verifies)
    fn unsandboxed_activates_declared_toolchains_on_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).expect("mkdir");
        let toolchains = vec![("t".to_owned(), bin)];
        let inputs = SandboxInputs {
            workdir: dir.path(),
            toolchains: &toolchains,
            command: "echo $PATH",
        };
        let output = UnsandboxedExecutor.run(&inputs).expect("runs");
        assert!(output.log.contains("bin"));
        let _: Vec<(String, PathBuf)> = toolchains;
    }
}
