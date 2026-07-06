//! Host-side materialization shared by every *local* effect backend (Docker,
//! and host-direct/`--unsandboxed`) — the Fly.io Sprite backend
//! ([`crate::engine`]) instead streams bytes into the Sprite's own
//! filesystem, since there is no host directory to bind-mount there.
//!
//! A [`Sandbox`] is one effect run's scratch area: a fresh temp directory
//! holding the checked-out tree (`work`), every declared toolchain's
//! extracted `bin` (`toolchains/<name>`), and every declared cache
//! (`cache/<name>`) — laid out on the *host* filesystem so the Docker backend
//! can bind-mount it straight into the container, and host-direct execution
//! can just point `PATH`/`$PWD` at it. Toolchain extraction goes through
//! [`git_toolchain::export`], the same function the Sprite path's own doc
//! comments call out as its local/hosted parity anchor, so a toolchain's
//! materialized bytes are identical no matter which backend runs it.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use gix_hash::ObjectId;
use std::process::Command;

use crate::definition::Effect;

/// One effect run's host-side scratch area, torn down when dropped.
pub struct Sandbox {
    root: tempfile::TempDir,
}

impl Sandbox {
    /// A fresh sandbox with empty `work`/`toolchains`/`cache` directories.
    pub fn new() -> Result<Self, String> {
        let root = tempfile::tempdir().map_err(|e| format!("could not create scratch dir: {e}"))?;
        for name in ["work", "toolchains", "cache"] {
            std::fs::create_dir_all(root.path().join(name))
                .map_err(|e| format!("could not create {name} dir: {e}"))?;
        }
        Ok(Self { root })
    }

    /// The checked-out tree's directory.
    #[must_use]
    pub fn work_dir(&self) -> PathBuf {
        self.root.path().join("work")
    }

    /// The parent of every extracted toolchain's `<name>` directory.
    #[must_use]
    pub fn toolchains_dir(&self) -> PathBuf {
        self.root.path().join("toolchains")
    }

    /// The parent of every restored cache's `<name>` directory.
    #[must_use]
    pub fn cache_root(&self) -> PathBuf {
        self.root.path().join("cache")
    }

    /// Where cache `name` is restored, created even absent a prior snapshot
    /// so a tool populating it fresh always finds it there.
    #[must_use]
    pub fn cache_dir(&self, name: &str) -> PathBuf {
        self.cache_root().join(name)
    }
}

/// Replace the sandbox's [`Sandbox::work_dir`] with the tree at `new`, via
/// `git archive | tar -x` straight onto the host filesystem — no sandbox CLI
/// involved, unlike the Sprite path's streamed unpack.
pub fn sync_tree(repo: &Path, sandbox: &Sandbox, new: ObjectId) -> Result<(), String> {
    let archive = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["archive", "--format=tar", &new.to_string()])
        .output()
        .map_err(|e| format!("could not run git archive: {e}"))?;
    if !archive.status.success() {
        return Err(format!("git archive failed for {new}"));
    }
    let work = sandbox.work_dir();
    let mut child = Command::new("tar")
        .args(["-x", "-C"])
        .arg(&work)
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run tar: {e}"))?;
    child
        .stdin
        .take()
        .ok_or("tar did not accept stdin")?
        .write_all(&archive.stdout)
        .map_err(|e| format!("could not extract the tree: {e}"))?;
    let status = child
        .wait()
        .map_err(|e| format!("tar did not complete: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("could not unpack the tree at {new}"))
    }
}

/// Resolve and extract every distinct toolchain named across `runnable` into
/// `sandbox.toolchains_dir()/<name>/bin` via [`git_toolchain::export`],
/// returning the resolved (deduplicated) names — the exported bytes are
/// identical regardless of whether `bin` is [`git_toolchain::Bin::Embedded`]
/// or [`git_toolchain::Bin::Downloaded`], since `export` normalizes both to
/// the same `<dest>/bin/…` shape.
pub fn resolve_toolchains(
    repo: &Path,
    sandbox: &Sandbox,
    runnable: &[Effect],
) -> Result<Vec<String>, String> {
    let mut names: Vec<&str> = runnable
        .iter()
        .flat_map(|effect| effect.toolchains.iter().map(String::as_str))
        .collect();
    names.sort_unstable();
    names.dedup();

    for name in &names {
        let dest = sandbox.toolchains_dir().join(name);
        if dest.exists() {
            continue;
        }
        git_toolchain::export(repo, name, &dest)
            .map_err(|e| format!("could not resolve toolchain {name}: {e}"))?;
    }
    Ok(names.into_iter().map(str::to_owned).collect())
}

/// The container/host-relative path a toolchain named `name` was exported to
/// (see [`resolve_toolchains`]), for building an `activate()` `PATH`.
#[must_use]
pub fn toolchain_bin_dir(sandbox: &Sandbox, name: &str) -> PathBuf {
    sandbox.toolchains_dir().join(name).join("bin")
}

/// A `name -> bin dir` map from `names`, each pointing at its host path under
/// `sandbox` — used by host-direct execution, which runs outside any
/// container and so needs the real host path rather than a bind-mounted
/// in-container one.
#[must_use]
pub fn host_toolchain_dirs(sandbox: &Sandbox, names: &[String]) -> HashMap<String, String> {
    names
        .iter()
        .map(|name| {
            (
                name.clone(),
                toolchain_bin_dir(sandbox, name).display().to_string(),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, reason = "unit test")]

    use super::*;

    // @relation(checks.sandbox, role=Verifies)
    #[test]
    fn sandbox_starts_with_empty_work_toolchains_cache() {
        let sandbox = Sandbox::new().unwrap();
        assert!(sandbox.work_dir().is_dir());
        assert!(sandbox.toolchains_dir().is_dir());
        assert!(sandbox.cache_root().is_dir());
    }

    // @relation(checks.sandbox, role=Verifies)
    #[test]
    fn host_toolchain_dirs_map_to_the_sandbox_bin_directory() {
        let sandbox = Sandbox::new().unwrap();
        let names = vec!["gcc".to_owned()];
        let dirs = host_toolchain_dirs(&sandbox, &names);
        assert_eq!(
            dirs["gcc"],
            sandbox
                .toolchains_dir()
                .join("gcc")
                .join("bin")
                .display()
                .to_string()
        );
    }
}
