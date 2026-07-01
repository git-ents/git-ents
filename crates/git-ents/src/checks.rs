//! The configured checks, sourced from the `refs/meta/checks` ref.
//!
//! A check is anything a server runs against a push — CI, CD, linting,
//! versioning gates, and so on. Their definitions live in exactly one place:
//! the `refs/meta/checks` ref. Its tree is a [`Checks`] document mapping each
//! check name to the [`CheckBody`] that runs it. The document is read and
//! written through [`git_store`], so the check set is a typed value that
//! lives in git — versioned, auditable, and itself pushable. Keeping it on a
//! meta ref rather than in the worktree means an untrusted branch cannot
//! rewrite the checks that gate it.
//!
//! # Migration note
//!
//! `Checks`/`RunResults` moved their map values from a bare `String` to a
//! struct (`CheckBody`/[`Outcome`]) so a run's outcome can carry more than one
//! field (a duration, a log URL). This turns `checks/<name>` and
//! `results/<name>` from blobs into subtrees on disk, an incompatible format
//! change: data written in the prior flat-string layout no longer loads and
//! must be re-recorded. Acceptable pre-1.0 (see the format compatibility
//! rules in `git_store`'s module docs).

use std::collections::BTreeMap;
use std::path::Path;

use facet::Facet;

/// The ref whose tree holds the configured check set.
pub const CHECKS_REF: &str = "refs/meta/checks";

/// A configured check's on-disk body. The map key (its name) is the check's
/// identity, so it is not duplicated inside the body.
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
struct CheckBody {
    /// The shell command run for the check (e.g. `cargo fmt --check`).
    command: String,
}

/// The document stored at [`CHECKS_REF`]: its `checks/` subtree maps each
/// check name to its [`CheckBody`].
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
struct Checks {
    checks: BTreeMap<String, CheckBody>,
}

/// One configured check, assembled from its map key and [`CheckBody`] at load.
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct Check {
    /// The name it is stored under.
    pub name: String,
    /// The shell command run for the check (e.g. `cargo fmt --check`).
    pub command: String,
}

/// Load the configured checks recorded at [`CHECKS_REF`] from an already-open
/// `store`.
///
/// An absent ref yields an empty set, as on a server whose check set has not
/// been pushed yet. A present but unreadable ref is an error so callers can
/// distinguish corruption from "no checks configured".
pub fn load(repo: &Path) -> Result<Vec<Check>, git_store::Error> {
    Ok(git_store::Store::open(repo)?
        .load::<Checks>(CHECKS_REF)?
        .map(|doc| {
            doc.checks
                .into_iter()
                .map(|(name, body)| Check {
                    name,
                    command: body.command,
                })
                .collect()
        })
        .unwrap_or_default())
}

/// Write `checks` to [`CHECKS_REF`] in `repo`, replacing any existing set as a
/// new commit.
pub fn store(repo: &Path, checks: &[Check]) -> Result<(), git_store::Error> {
    let doc = Checks {
        checks: checks
            .iter()
            .cloned()
            .map(|check| {
                (
                    check.name,
                    CheckBody {
                        command: check.command,
                    },
                )
            })
            .collect(),
    };
    git_store::Store::open(repo)?.store(CHECKS_REF, &doc, "Update checks")
}

/// The namespace under which a commit's check runs are recorded: one ref,
/// `refs/meta/runs/<commit>`, per checked commit, holding the *log* of every
/// run against it. Definitions live on [`CHECKS_REF`]; this is their history.
pub const RUNS_NS: &str = "refs/meta/runs";

/// One check's on-disk outcome. The map key (the check's name) is not
/// duplicated inside it. Optional fields absent from an older record load as
/// unset, so a run recorded before a field existed still loads.
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
struct Outcome {
    /// `queued`, `running`, then `pass`, `fail`, or `error`.
    outcome: String,
    /// How long the check took to run, when known.
    duration_secs: Option<u64>,
    /// Where to read the check's full log, when the runner published one.
    log_url: Option<String>,
}

/// One run's outcomes, stored as the tree of a commit on the run ref:
/// `results/<name>` maps each check to its [`Outcome`]. Each commit on the ref
/// is one run and the commit's date is when it ran, so no timestamp is
/// duplicated in the tree — the run history is the ref's commit chain.
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
struct RunResults {
    results: BTreeMap<String, Outcome>,
}

/// One check's outcome within a [`Run`], assembled from its map key and
/// [`Outcome`] at load.
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct RunOutcome {
    /// The check's name (its `checks/<name>` in [`CHECKS_REF`]).
    pub name: String,
    /// The outcome recorded for it as a run progresses — `queued`, `running`,
    /// then `pass`, `fail`, or `error`.
    pub outcome: String,
    /// How long the check took to run, when known.
    pub duration_secs: Option<u64>,
    /// Where to read the check's full log, when the runner published one.
    pub log_url: Option<String>,
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

/// Record a run of `outcomes` for `commit` in `repo`, as a new commit on
/// `refs/meta/runs/<commit>`, parented on the prior run so the ref's commit
/// chain is the run history. The commit's date is the run time.
pub fn record(repo: &Path, commit: &str, outcomes: &[RunOutcome]) -> Result<(), git_store::Error> {
    git_store::Store::open(repo)?.store(
        &format!("{RUNS_NS}/{commit}"),
        &run_doc(outcomes),
        "Record check run",
    )
}

/// Advance the latest run recorded for `commit` to `outcomes`, in place, in
/// `repo`. Unlike [`record`], which appends a new run, this replaces the run
/// ref's tip commit (re-parented on the prior run) so a single run's status can
/// progress — `queued` → `running` → results — without appending a commit per
/// transition.
///
/// When no run has been recorded yet the update starts one, so a worker that
/// advances a run is self-healing even if the `queued` record never landed.
pub fn update_run(
    repo: &Path,
    commit: &str,
    outcomes: &[RunOutcome],
) -> Result<(), git_store::Error> {
    git_store::Store::open(repo)?.amend(
        &format!("{RUNS_NS}/{commit}"),
        &run_doc(outcomes),
        "Record check run",
    )
}

/// List the recorded runs per commit in `repo`, newest commit first. Each
/// commit's runs are the ref's commit chain, newest first, with the run time
/// taken from each commit's date.
pub fn runs(repo: &Path) -> Result<Vec<CommitRuns>, git_store::Error> {
    let store = git_store::Store::open(repo)?;
    let prefix = format!("{RUNS_NS}/");
    let mut commits = Vec::new();
    for refname in store.list(&prefix)? {
        let Some(commit) = refname.strip_prefix(&prefix) else {
            continue;
        };
        let runs = store
            .history::<RunResults>(&refname)?
            .into_iter()
            .map(|(at, doc)| Run {
                at,
                results: doc
                    .results
                    .into_iter()
                    .map(|(name, outcome)| assemble_outcome(name, outcome))
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

/// Build a [`RunResults`] from a run's `outcomes`.
fn run_doc(outcomes: &[RunOutcome]) -> RunResults {
    RunResults {
        results: outcomes
            .iter()
            .cloned()
            .map(|outcome| {
                (
                    outcome.name,
                    Outcome {
                        outcome: outcome.outcome,
                        duration_secs: outcome.duration_secs,
                        log_url: outcome.log_url,
                    },
                )
            })
            .collect(),
    }
}

/// Assemble a public [`RunOutcome`] from its map key and on-disk [`Outcome`].
fn assemble_outcome(name: String, outcome: Outcome) -> RunOutcome {
    RunOutcome {
        name,
        outcome: outcome.outcome,
        duration_secs: outcome.duration_secs,
        log_url: outcome.log_url,
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

    use super::*;
    use crate::testutil::{unique_repo as new_repo, write_checks_doc, write_runs_doc};

    fn unique_repo() -> std::path::PathBuf {
        new_repo("checks")
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

    #[test]
    fn loads_the_on_disk_checks_format() {
        // A fixture written as the real `checks/<name>/command` subtree layout
        // (the 2c migration: a struct value, not a bare blob) must keep
        // loading, guarding the Checks document's shape against an
        // incompatible change to data already on a ref.
        let repo = unique_repo();
        write_checks_doc(
            &repo,
            &[("fmt", "cargo fmt --check"), ("test", "cargo nextest run")],
        );
        let mut loaded = load(&repo).unwrap();
        loaded.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(
            loaded,
            vec![
                check("fmt", "cargo fmt --check"),
                check("test", "cargo nextest run")
            ]
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn loads_the_on_disk_runs_format() {
        // A fixture written as the real `results/<name>/outcome` subtree
        // layout (the 2c migration) with `duration_secs`/`log_url` omitted
        // must keep loading, with the missing optional fields unset.
        let repo = unique_repo();
        let commit = "0123456789012345678901234567890123456789";
        write_runs_doc(
            &repo,
            &format!("{RUNS_NS}/{commit}"),
            &[("fmt", "pass"), ("test", "fail")],
        );
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

    fn outcome(name: &str, outcome: &str) -> RunOutcome {
        RunOutcome {
            name: name.to_owned(),
            outcome: outcome.to_owned(),
            duration_secs: None,
            log_url: None,
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

    #[test]
    fn round_trips_an_outcomes_duration_and_log_url() {
        let repo = unique_repo();
        let commit = "0123456789012345678901234567890123456789";
        let rich = RunOutcome {
            name: "fmt".to_owned(),
            outcome: "pass".to_owned(),
            duration_secs: Some(12),
            log_url: Some("https://example.com/log".to_owned()),
        };
        record(&repo, commit, std::slice::from_ref(&rich)).unwrap();
        let commits = runs(&repo).unwrap();
        assert_eq!(commits[0].runs[0].results, vec![rich]);
        let _ = std::fs::remove_dir_all(&repo);
    }
}
