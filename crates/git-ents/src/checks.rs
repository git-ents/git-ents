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

/// The namespace under which a push's check outcomes are recorded: one ref,
/// `refs/checks/<commit>`, per checked commit. The ref points at a commit whose
/// tree is a [`RunDoc`] — a recorded run *is* a git ref.
pub const RESULTS_NS: &str = "refs/checks";

/// The recorded outcomes for one checked commit, stored at the run's ref:
/// `results/<name>` maps to that check's outcome.
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
struct RunDoc {
    results: BTreeMap<String, String>,
}

/// One check's outcome within a [`CheckRun`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutcome {
    /// The check's name (its `checks/<name>` in [`CHECKS_REF`]).
    pub name: String,
    /// The outcome recorded for it — `pass`, `fail`, or `error`.
    pub outcome: String,
}

/// A recorded run: the commit that was checked and each check's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckRun {
    /// The checked commit's object id.
    pub commit: String,
    /// Each check's outcome, in name order.
    pub results: Vec<RunOutcome>,
}

/// Record `outcomes` for `commit` at `refs/checks/<commit>`, replacing any
/// previous run for that commit, as a new commit. The outcomes are written as a
/// [`RunDoc`] git tree through [`facet_git_tree`], so the run is a typed value
/// living in git, like the check set itself.
pub fn record(repo: &Path, commit: &str, outcomes: &[RunOutcome]) -> Result<(), Error> {
    let doc = RunDoc {
        results: outcomes
            .iter()
            .map(|outcome| (outcome.name.clone(), outcome.outcome.clone()))
            .collect(),
    };
    let odb = open_odb(repo).ok_or(Error::Odb)?;
    let tree = facet_git_tree::serialize_into(&doc, &odb)?;
    let refname = format!("{RESULTS_NS}/{commit}");
    let parent = ref_commit(repo, &refname);
    let new_commit = commit_run(repo, &tree, parent.as_deref())?;
    update_named_ref(repo, &refname, &new_commit)
}

/// List the recorded runs, newest first.
pub fn runs(repo: &Path) -> Result<Vec<CheckRun>, Error> {
    let refs = run_refs(repo)?;
    if refs.is_empty() {
        return Ok(Vec::new());
    }
    let odb = open_odb(repo).ok_or(Error::Odb)?;
    let prefix = format!("{RESULTS_NS}/");
    let mut runs = Vec::new();
    for refname in refs {
        let Some(commit) = refname.strip_prefix(&prefix) else {
            continue;
        };
        let Some(tree) = ref_tree(repo, &refname) else {
            continue;
        };
        let doc: RunDoc = facet_git_tree::deserialize(&tree, &odb)?;
        runs.push(CheckRun {
            commit: commit.to_owned(),
            results: doc
                .results
                .into_iter()
                .map(|(name, outcome)| RunOutcome { name, outcome })
                .collect(),
        });
    }
    Ok(runs)
}

/// List the `refs/checks/*` refs, newest committed first.
fn run_refs(repo: &Path) -> Result<Vec<String>, Error> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname)",
            RESULTS_NS,
        ])
        .output()
        .map_err(|_source| Error::Git {
            operation: "for-each-ref",
        })?;
    if !output.status.success() {
        return Err(Error::Git {
            operation: "for-each-ref",
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect())
}

/// Resolve `refname` to the object id of its tree, or `None` when it is absent.
fn ref_tree(repo: &Path, refname: &str) -> Option<ObjectId> {
    let spec = format!("{refname}^{{tree}}");
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

/// Resolve `refname` to the object id of its commit, or `None` when it is
/// absent.
fn ref_commit(repo: &Path, refname: &str) -> Option<String> {
    let spec = format!("{refname}^{{commit}}");
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

/// Wrap a run `tree` in a commit, parenting on the run's previous ref when
/// present so re-runs accrue history. A fixed identity keeps the write
/// self-contained, independent of any ambient git config.
fn commit_run(repo: &Path, tree: &ObjectId, parent: Option<&str>) -> Result<String, Error> {
    let mut args = vec!["commit-tree".to_owned(), tree.to_string()];
    if let Some(parent) = parent {
        args.push("-p".to_owned());
        args.push(parent.to_owned());
    }
    args.push("-m".to_owned());
    args.push("Record check run".to_owned());
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

/// Point `refname` at `commit`.
fn update_named_ref(repo: &Path, refname: &str, commit: &str) -> Result<(), Error> {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["update-ref", refname, commit])
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
        clippy::indexing_slicing,
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

    fn outcome(name: &str, outcome: &str) -> RunOutcome {
        RunOutcome {
            name: name.to_owned(),
            outcome: outcome.to_owned(),
        }
    }

    #[test]
    fn record_then_runs_round_trips_a_run() {
        let repo = unique_repo();
        let commit = "0123456789012345678901234567890123456789";
        record(
            &repo,
            commit,
            &[outcome("fmt", "pass"), outcome("test", "fail")],
        )
        .unwrap();

        let runs = runs(&repo).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].commit, commit);
        assert_eq!(
            runs[0].results,
            vec![outcome("fmt", "pass"), outcome("test", "fail")]
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn recording_a_commit_again_replaces_its_run() {
        let repo = unique_repo();
        let commit = "0123456789012345678901234567890123456789";
        record(&repo, commit, &[outcome("fmt", "fail")]).unwrap();
        record(&repo, commit, &[outcome("fmt", "pass")]).unwrap();
        let runs = runs(&repo).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].results, vec![outcome("fmt", "pass")]);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn empty_when_no_runs_recorded() {
        let repo = unique_repo();
        assert!(runs(&repo).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&repo);
    }
}
