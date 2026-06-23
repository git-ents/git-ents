//! The configured checks, sourced from the `refs/meta/checks` ref.
//!
//! A check is anything a server runs against a push — CI, CD, linting,
//! versioning gates, and so on. Their definitions live in exactly one place:
//! the `refs/meta/checks` ref. Its tree is a [`Checks`] document mapping each
//! check name to the command that runs it. The document is read and written
//! through [`git_store`], so the check set is a typed value that lives in git —
//! versioned, auditable, and itself pushable. Keeping it on a meta ref rather
//! than in the worktree means an untrusted branch cannot rewrite the checks
//! that gate it.

use std::collections::BTreeMap;
use std::path::Path;

use facet::Facet;

/// The ref whose tree holds the configured check set.
pub const CHECKS_REF: &str = "refs/meta/checks";

/// The check document stored at [`CHECKS_REF`]: its `checks/` subtree maps each
/// check name to that check's definition (its command).
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
struct Checks {
    checks: BTreeMap<String, CheckDef>,
}

/// One check's stored definition. A struct (rather than a bare command blob) so
/// each check can grow per-check settings without a tree-format migration.
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
struct CheckDef {
    /// The shell command run for the check.
    command: String,
}

/// One configured check recorded in [`CHECKS_REF`].
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct Check {
    /// The name it is stored under.
    pub name: String,
    /// The shell command run for the check (e.g. `cargo fmt --check`).
    pub command: String,
}

/// A failure reading or writing the check set.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The check set could not be read from or written to its ref.
    #[error(transparent)]
    Store(#[from] git_store::Error),
}

/// Load the configured checks recorded at [`CHECKS_REF`] in `repo`.
///
/// An absent ref yields an empty set, as on a server whose check set has not
/// been pushed yet. A present but unreadable ref is an error so callers can
/// distinguish corruption from "no checks configured".
pub fn load(repo: &Path) -> Result<Vec<Check>, Error> {
    let store = git_store::Store::open(repo)?;
    let Some(document) = store.load::<Checks>(CHECKS_REF)? else {
        return Ok(Vec::new());
    };
    Ok(document
        .checks
        .into_iter()
        .map(|(name, def)| Check {
            name,
            command: def.command.trim_end().to_owned(),
        })
        .collect())
}

/// Write `checks` to [`CHECKS_REF`], replacing any existing set, as a new
/// commit.
pub fn store(repo: &Path, checks: &[Check]) -> Result<(), Error> {
    let document = Checks {
        checks: checks
            .iter()
            .map(|check| {
                (
                    check.name.clone(),
                    CheckDef {
                        command: check.command.clone(),
                    },
                )
            })
            .collect(),
    };
    git_store::Store::open(repo)?.store(CHECKS_REF, &document, "Update checks")?;
    Ok(())
}

/// The namespace under which a commit's check runs are recorded: one ref,
/// `refs/meta/runs/<commit>`, per checked commit, holding the *log* of every
/// run against it. Definitions live on [`CHECKS_REF`]; this is their history.
pub const RUNS_NS: &str = "refs/meta/runs";

/// One run's outcomes, stored as the tree of a commit on the run ref:
/// `results/<name>` maps each check to its outcome. Each commit on the ref is
/// one run and the commit's date is when it ran, so no timestamp is duplicated
/// in the tree — the run history is the ref's commit chain.
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
struct RunDoc {
    results: BTreeMap<String, String>,
}

/// One check's outcome within a [`Run`].
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct RunOutcome {
    /// The check's name (its `checks/<name>` in [`CHECKS_REF`]).
    pub name: String,
    /// The outcome recorded for it as a run progresses — `queued`, `running`,
    /// then `pass`, `fail`, or `error`.
    pub outcome: String,
}

/// One recorded execution of the check set against a commit.
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct Run {
    /// When the run was recorded, as seconds since the Unix epoch — the run
    /// commit's committer date.
    pub at: u64,
    /// Each check's outcome, in name order.
    pub results: Vec<RunOutcome>,
}

/// The runs recorded for one commit: its object id and every execution against
/// it, newest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRuns {
    /// The checked commit's object id.
    pub commit: String,
    /// Every run against it, newest first.
    pub runs: Vec<Run>,
}

/// Record a run of `outcomes` for `commit` as a new commit on
/// `refs/meta/runs/<commit>`, parented on the prior run so the ref's commit
/// chain is the run history. The commit's date is the run time.
pub fn record(repo: &Path, commit: &str, outcomes: &[RunOutcome]) -> Result<(), Error> {
    git_store::Store::open(repo)?.store(
        &format!("{RUNS_NS}/{commit}"),
        &run_doc(outcomes),
        "Record check run",
    )?;
    Ok(())
}

/// Advance the latest run recorded for `commit` to `outcomes`, in place. Unlike
/// [`record`], which appends a new run, this replaces the run ref's tip commit
/// (re-parented on the prior run) so a single run's status can progress —
/// `queued` → `running` → results — without appending a commit per transition.
///
/// When no run has been recorded yet the update starts one, so a worker that
/// advances a run is self-healing even if the `queued` record never landed.
pub fn update_run(repo: &Path, commit: &str, outcomes: &[RunOutcome]) -> Result<(), Error> {
    git_store::Store::open(repo)?.amend(
        &format!("{RUNS_NS}/{commit}"),
        &run_doc(outcomes),
        "Record check run",
    )?;
    Ok(())
}

/// List the recorded runs per commit, newest commit first. Each commit's runs
/// are the ref's commit chain, newest first, with the run time taken from each
/// commit's date.
pub fn runs(repo: &Path) -> Result<Vec<CommitRuns>, Error> {
    let store = git_store::Store::open(repo)?;
    let prefix = format!("{RUNS_NS}/");
    let mut commits = Vec::new();
    for refname in store.list(&prefix)? {
        let Some(commit) = refname.strip_prefix(&prefix) else {
            continue;
        };
        let runs = store
            .history::<RunDoc>(&refname)?
            .into_iter()
            .map(|(at, doc)| Run {
                at,
                results: doc
                    .results
                    .into_iter()
                    .map(|(name, outcome)| RunOutcome { name, outcome })
                    .collect(),
            })
            .collect();
        commits.push(CommitRuns {
            commit: commit.to_owned(),
            runs,
        });
    }
    Ok(commits)
}

/// Build a [`RunDoc`] from a run's `outcomes`.
fn run_doc(outcomes: &[RunOutcome]) -> RunDoc {
    RunDoc {
        results: outcomes
            .iter()
            .map(|outcome| (outcome.name.clone(), outcome.outcome.clone()))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::indexing_slicing,
        clippy::let_underscore_must_use,
        reason = "unit test"
    )]

    use std::path::PathBuf;
    use std::process::Command;
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

        let commits = runs(&repo).unwrap();
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].commit, commit);
        assert_eq!(commits[0].runs.len(), 1);
        assert_eq!(
            commits[0].runs[0].results,
            vec![outcome("fmt", "pass"), outcome("test", "fail")]
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn recording_a_commit_again_appends_a_run() {
        let repo = unique_repo();
        let commit = "0123456789012345678901234567890123456789";
        record(&repo, commit, &[outcome("fmt", "fail")]).unwrap();
        record(&repo, commit, &[outcome("fmt", "pass")]).unwrap();
        let commits = runs(&repo).unwrap();
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].runs.len(), 2);
        // Newest first: the second run (pass) leads, the first (fail) follows.
        assert_eq!(commits[0].runs[0].results, vec![outcome("fmt", "pass")]);
        assert_eq!(commits[0].runs[1].results, vec![outcome("fmt", "fail")]);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn empty_when_no_runs_recorded() {
        let repo = unique_repo();
        assert!(runs(&repo).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&repo);
    }
}
