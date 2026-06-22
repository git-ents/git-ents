//! The configured checks, sourced from the `refs/meta/checks` ref.
//!
//! A check is anything a server runs against a push — CI, CD, linting,
//! versioning gates, and so on. Their definitions live in exactly one place:
//! the `refs/meta/checks` ref. Its tree is a [`Checks`] document whose `checks/`
//! subtree maps each check name to the command that runs it. The document is
//! read and written with [`facet_git_tree`], so the check set is a typed value
//! that lives in git — versioned, auditable, and itself pushable. Keeping it on
//! a meta ref rather than in the worktree means an untrusted branch cannot
//! rewrite the checks that gate it.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use facet::Facet;
use facet_git_tree::ObjectId;

/// The ref whose tree holds the configured check set.
pub const CHECKS_REF: &str = "refs/meta/checks";

/// The check document stored at [`CHECKS_REF`]: `checks/<name>` maps to the
/// command run for that check.
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
struct Checks {
    checks: BTreeMap<String, String>,
}

/// One configured check recorded under `checks/` in [`CHECKS_REF`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    /// The `checks/<name>` the command is stored under — the check's name.
    pub name: String,
    /// The shell command run for the check (e.g. `cargo fmt --check`).
    pub command: String,
}

/// A failure reading or writing the check set.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The repository's object database could not be opened.
    #[error("could not open the repository object database")]
    Odb,
    /// The check set could not be (de)serialized from its git tree.
    #[error("could not (de)serialize the check set: {0}")]
    Facet(#[from] facet_git_tree::Error),
    /// A git invocation needed to read or update the ref failed.
    #[error("git {operation} failed")]
    Git {
        /// The git operation that failed.
        operation: &'static str,
    },
}

/// Load the configured checks recorded at [`CHECKS_REF`] in `repo`.
///
/// An absent ref yields an empty set, as on a server whose check set has not
/// been pushed yet. A present but unreadable ref is an error so callers can
/// distinguish corruption from "no checks configured".
pub fn load(repo: &Path) -> Result<Vec<Check>, Error> {
    let Some(tree) = checks_tree(repo) else {
        return Ok(Vec::new());
    };
    let odb = open_odb(repo).ok_or(Error::Odb)?;
    let checks: Checks = facet_git_tree::deserialize(&tree, &odb)?;
    Ok(checks
        .checks
        .into_iter()
        .map(|(name, command)| Check {
            name,
            command: command.trim_end().to_owned(),
        })
        .collect())
}

/// Write `checks` to [`CHECKS_REF`], replacing any existing set, as a new
/// commit.
pub fn store(repo: &Path, checks: &[Check]) -> Result<(), Error> {
    let document = Checks {
        checks: checks
            .iter()
            .map(|check| (check.name.clone(), check.command.clone()))
            .collect(),
    };
    let odb = open_odb(repo).ok_or(Error::Odb)?;
    let tree = facet_git_tree::serialize_into(&document, &odb)?;
    let commit = commit_tree(repo, &tree)?;
    update_ref(repo, &commit)
}

/// Resolve [`CHECKS_REF`] to the object id of its tree, or `None` when the ref
/// is absent.
fn checks_tree(repo: &Path) -> Option<ObjectId> {
    let spec = format!("{CHECKS_REF}^{{tree}}");
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", "--quiet", &spec])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let hex = String::from_utf8(output.stdout).ok()?;
    ObjectId::from_hex(hex.trim().as_bytes()).ok()
}

/// Open the repository's durable object database as a `gix` `Find`/`Write`
/// backend.
///
/// Resolves the *common* git directory rather than `--git-path objects` so that
/// inside a hook the durable store is read, never a receive-pack quarantine.
fn open_odb(repo: &Path) -> Option<gix_odb::Handle> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--git-common-dir"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let git_dir = String::from_utf8(output.stdout).ok()?;
    gix_odb::at(repo.join(git_dir.trim()).join("objects")).ok()
}

/// Wrap `tree` in a commit, returning its object id. The commit parents on the
/// current [`CHECKS_REF`] when present so updates fast-forward and accrue
/// history; a fixed identity keeps the write self-contained, independent of any
/// ambient git config.
fn commit_tree(repo: &Path, tree: &ObjectId) -> Result<String, Error> {
    let mut args = vec!["commit-tree".to_owned(), tree.to_string()];
    if let Some(parent) = checks_commit(repo) {
        args.push("-p".to_owned());
        args.push(parent);
    }
    args.push("-m".to_owned());
    args.push("Update checks".to_owned());
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(&args)
        .env("GIT_AUTHOR_NAME", "git-ents")
        .env("GIT_AUTHOR_EMAIL", "git-ents@localhost")
        .env("GIT_COMMITTER_NAME", "git-ents")
        .env("GIT_COMMITTER_EMAIL", "git-ents@localhost")
        .output()
        .map_err(|_source| Error::Git {
            operation: "commit-tree",
        })?;
    if !output.status.success() {
        return Err(Error::Git {
            operation: "commit-tree",
        });
    }
    String::from_utf8(output.stdout)
        .map(|stdout| stdout.trim().to_owned())
        .map_err(|_invalid| Error::Git {
            operation: "commit-tree",
        })
}

/// Resolve [`CHECKS_REF`] to the object id of its commit, or `None` when the ref
/// is absent.
fn checks_commit(repo: &Path) -> Option<String> {
    let spec = format!("{CHECKS_REF}^{{commit}}");
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", "--quiet", &spec])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let hex = String::from_utf8(output.stdout).ok()?;
    let hex = hex.trim();
    if hex.is_empty() {
        None
    } else {
        Some(hex.to_owned())
    }
}

/// Point [`CHECKS_REF`] at `commit`.
fn update_ref(repo: &Path, commit: &str) -> Result<(), Error> {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["update-ref", CHECKS_REF, commit])
        .status()
        .map_err(|_source| Error::Git {
            operation: "update-ref",
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::Git {
            operation: "update-ref",
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::panic,
        clippy::arithmetic_side_effects,
        clippy::let_underscore_must_use,
        reason = "unit test"
    )]

    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    fn unique_repo() -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("git-ents-checks-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let status = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["init", "-q"])
            .status()
            .unwrap();
        assert!(status.success());
        dir
    }

    fn check(name: &str, command: &str) -> Check {
        Check {
            name: name.to_owned(),
            command: command.to_owned(),
        }
    }

    #[test]
    fn store_then_load_round_trips_the_check_set() {
        let repo = unique_repo();
        let written = vec![
            check("fmt", "cargo fmt --check"),
            check("test", "cargo nextest run"),
        ];
        store(&repo, &written).unwrap();

        let mut loaded = load(&repo).unwrap();
        loaded.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(loaded, written);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn store_replaces_the_previous_set() {
        let repo = unique_repo();
        store(&repo, &[check("fmt", "cargo fmt --check")]).unwrap();
        store(&repo, &[check("test", "cargo nextest run")]).unwrap();
        assert_eq!(
            load(&repo).unwrap(),
            vec![check("test", "cargo nextest run")]
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn empty_when_the_checks_ref_is_absent() {
        let repo = unique_repo();
        assert!(load(&repo).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&repo);
    }
}
