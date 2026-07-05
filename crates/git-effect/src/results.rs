//! Recorded effect runs, sourced from `refs/meta/results/<effect>/<short-oid>`
//! — one ref per effect, per checked commit.
//!
//! # Migration note
//!
//! Results were runs: `refs/meta/runs/<commit>` (one ref per commit, a
//! scalar-keyed map of every check's outcome) decomposed to
//! `refs/meta/results/<effect>/<short-oid>` (one ref per effect per commit),
//! matching [`crate::definition`]'s checks→effects decomposition. The public
//! [`CommitRuns`]/[`Run`]/[`RunOutcome`] shape stays the aggregate view a
//! caller wants — every effect's outcome against a commit, grouped by the
//! moment they were recorded — reassembled in [`runs`] from the decomposed
//! refs rather than read directly off one ref. Incompatible with data written
//! in the prior layout — acceptable pre-1.0 (see the format compatibility
//! rules in `git_store`'s module docs).

use std::path::Path;

use facet::Facet;
use gix_hash::ObjectId;

/// The ref namespace under which effect runs are recorded: one ref,
/// `refs/meta/results/<effect>/<short-oid>`, per effect per checked commit,
/// holding the *log* of every run of that effect against that commit.
/// Definitions live under [`crate::definition::EFFECTS_NS`]; this is their
/// history.
pub const RESULTS_NS: &str = "refs/meta/results";

/// How many hex characters of the checked commit's id the ref's last segment
/// carries. The full id is also stored in the document body (see
/// [`ResultBody::commit`]), so truncation here is purely a naming
/// convenience, not a loss of precision.
const SHORT_LEN: usize = 12;

/// An effect run's status, progressing `Queued` → `Running` → a terminal
/// outcome. Closed set — the only values a run legitimately takes, in place
/// of a `String` that every caller had to trust held one of five values.
///
/// ## Requirements
///
/// @relation(checks.outcomes)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Facet)]
#[repr(u8)]
pub enum Status {
    /// Enqueued by `post-receive`, not yet picked up by the worker.
    Queued,
    /// The worker has started this run.
    Running,
    /// The effect exited successfully.
    Pass,
    /// The effect exited with a failure.
    Fail,
    /// An infrastructure failure (an unreachable sandbox, a timeout) kept the
    /// effect from completing.
    Error,
    /// The effect never ran because a dependency did not pass.
    Skipped,
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Error => "error",
            Self::Skipped => "skipped",
        })
    }
}

/// One effect run's on-disk body, at `refs/meta/results/<effect>/<short-oid>`.
/// The checked commit's full id is carried here (not just abbreviated in the
/// ref name) so [`runs`] can recover it exactly regardless of [`SHORT_LEN`].
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
struct ResultBody {
    /// The checked commit's full hex id.
    commit: String,
    /// `queued`, `running`, then `pass`, `fail`, or `error`.
    status: Status,
    /// How long the effect took to run, when known.
    duration_secs: Option<u64>,
    /// The effect's terminal session, captured as asciicast v2 (JSONL) text,
    /// when the runner recorded one.
    recording: Option<String>,
    /// The command's process exit code, when the effect ran to completion
    /// rather than erroring out before or during execution (an unreachable
    /// sandbox, a timeout).
    exit_code: Option<i32>,
}

/// One effect's outcome, independent of which commit or moment it was
/// recorded for.
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct RunOutcome {
    /// The effect's name (its `refs/meta/effects/<name>`).
    pub name: String,
    /// The outcome recorded for it as a run progresses.
    pub status: Status,
    /// How long the effect took to run, when known.
    pub duration_secs: Option<u64>,
    /// The effect's terminal session, captured as asciicast v2 (JSONL) text,
    /// when the runner recorded one.
    pub recording: Option<String>,
    /// The command's process exit code, when the effect ran to completion
    /// rather than erroring out before or during execution (an unreachable
    /// sandbox, a timeout).
    pub exit_code: Option<i32>,
}

/// One recorded execution of the effect set against a commit — every effect's
/// outcome recorded at the same moment, reassembled from their independent
/// per-effect refs (see the module's migration note).
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct Run {
    /// When the run was recorded, as seconds since the Unix epoch — the
    /// underlying commits' committer date.
    pub at: u64,
    /// Each effect's outcome recorded at `at`, in name order.
    pub results: Vec<RunOutcome>,
}

/// The runs recorded for one commit: its object id and every execution
/// against it, newest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRuns {
    /// The checked commit's object id.
    pub commit: ObjectId,
    /// Every run against it, newest first.
    pub runs: Vec<Run>,
}

/// The ref holding effect `effect`'s run history against `commit`.
fn result_ref(effect: &str, commit: ObjectId) -> String {
    format!(
        "{RESULTS_NS}/{effect}/{}",
        commit.to_hex_with_len(SHORT_LEN)
    )
}

/// Record a run of `outcomes` against `commit` in `repo`: each effect's
/// outcome becomes a new commit on its own `result_ref`, parented on that
/// effect's prior run, so each effect's ref accrues its own history. Not
/// atomic across effects — an effect's own ref is the unit of consistency
/// here, the same one-ref-per-entity trade-off [`crate::definition`] makes.
///
/// ## Requirements
///
/// @relation(checks.outcomes)
pub fn record(
    repo: &Path,
    commit: ObjectId,
    outcomes: &[RunOutcome],
) -> Result<(), git_store::Error> {
    let store = git_store::Store::open(repo)?;
    for outcome in outcomes {
        let body = to_body(commit, outcome);
        store.store(
            &result_ref(&outcome.name, commit),
            &body,
            "Record effect run",
        )?;
    }
    Ok(())
}

/// Advance the latest run recorded for each of `outcomes`' effects against
/// `commit`, in place. Unlike [`record`], which appends a new run per effect,
/// this replaces each effect's run ref tip (re-parented on its prior parents)
/// so a single run's status can progress — `queued` → `running` → results —
/// without appending a commit per transition.
///
/// When no run has been recorded yet for an effect the update starts one, so
/// a worker that advances a run is self-healing even if the `queued` record
/// never landed.
///
/// ## Requirements
///
/// @relation(checks.outcomes)
pub fn update_run(
    repo: &Path,
    commit: ObjectId,
    outcomes: &[RunOutcome],
) -> Result<(), git_store::Error> {
    let store = git_store::Store::open(repo)?;
    for outcome in outcomes {
        let body = to_body(commit, outcome);
        store.amend(
            &result_ref(&outcome.name, commit),
            &body,
            "Record effect run",
        )?;
    }
    Ok(())
}

/// List the recorded runs per commit in `repo`, newest commit first. Every
/// effect's run history under [`RESULTS_NS`] is read and grouped by checked
/// commit, then by the recorded moment (`at`), so effects updated together
/// (as the worker always does — see [`update_run`]) reassemble into one
/// [`Run`] with every effect's outcome, matching the pre-decomposition shape.
///
/// A ref whose path does not decompose into `<effect>/<short-oid>`, or whose
/// commit segment is not a valid hex object id, cannot have been written by
/// [`record`]/[`update_run`], so it is skipped rather than surfaced as an
/// error.
pub fn runs(repo: &Path) -> Result<Vec<CommitRuns>, git_store::Error> {
    let store = git_store::Store::open(repo)?;
    let prefix = format!("{RESULTS_NS}/");
    let mut by_commit: std::collections::BTreeMap<
        ObjectId,
        std::collections::BTreeMap<u64, Vec<RunOutcome>>,
    > = std::collections::BTreeMap::new();
    for refname in store.list(&prefix)? {
        let Some(rest) = refname.strip_prefix(&prefix) else {
            continue;
        };
        let Some((effect, _short_oid)) = rest.split_once('/') else {
            continue;
        };
        for (at, body) in store.history::<ResultBody>(&refname)? {
            let Some(commit) = ObjectId::from_hex(body.commit.as_bytes()).ok() else {
                continue;
            };
            by_commit
                .entry(commit)
                .or_default()
                .entry(at)
                .or_default()
                .push(from_body(effect.to_owned(), body));
        }
    }

    let mut commits: Vec<CommitRuns> = by_commit
        .into_iter()
        .map(|(commit, by_at)| {
            let mut runs: Vec<Run> = by_at
                .into_iter()
                .map(|(at, mut results)| {
                    results.sort_by(|a, b| a.name.cmp(&b.name));
                    Run { at, results }
                })
                .collect();
            runs.sort_by_key(|run| std::cmp::Reverse(run.at));
            CommitRuns { commit, runs }
        })
        .collect();
    commits.sort_by(|a, b| {
        let a_at = a.runs.first().map_or(0, |run| run.at);
        let b_at = b.runs.first().map_or(0, |run| run.at);
        b_at.cmp(&a_at)
    });
    Ok(commits)
}

/// Build a [`ResultBody`] from a public [`RunOutcome`] for `commit`.
fn to_body(commit: ObjectId, outcome: &RunOutcome) -> ResultBody {
    ResultBody {
        commit: commit.to_string(),
        status: outcome.status,
        duration_secs: outcome.duration_secs,
        recording: outcome.recording.clone(),
        exit_code: outcome.exit_code,
    }
}

/// Assemble a public [`RunOutcome`] named `name` from its on-disk [`ResultBody`].
fn from_body(name: String, body: ResultBody) -> RunOutcome {
    RunOutcome {
        name,
        status: body.status,
        duration_secs: body.duration_secs,
        recording: body.recording,
        exit_code: body.exit_code,
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
    use crate::testutil::{unique_repo as new_repo, write_result_doc};

    fn unique_repo() -> std::path::PathBuf {
        new_repo("results")
    }

    fn outcome(name: &str, status: Status) -> RunOutcome {
        RunOutcome {
            name: name.to_owned(),
            status,
            duration_secs: None,
            recording: None,
            exit_code: None,
        }
    }

    // @relation(checks.outcomes, role=Verifies)
    #[test]
    fn record_then_runs_round_trips_a_run() {
        let repo = unique_repo();
        let commit = ObjectId::from_hex(b"0123456789012345678901234567890123456789").unwrap();
        record(
            &repo,
            commit,
            &[outcome("fmt", Status::Pass), outcome("test", Status::Fail)],
        )
        .unwrap();

        let commits = runs(&repo).unwrap();
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].commit, commit);
        assert_eq!(commits[0].runs.len(), 1);
        assert_eq!(
            commits[0].runs[0].results,
            vec![outcome("fmt", Status::Pass), outcome("test", Status::Fail)]
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    // @relation(checks.outcomes, role=Verifies)
    #[test]
    fn update_run_advances_in_place_rather_than_appending() {
        let repo = unique_repo();
        let commit = ObjectId::from_hex(b"0123456789012345678901234567890123456789").unwrap();
        record(&repo, commit, &[outcome("fmt", Status::Queued)]).unwrap();
        update_run(&repo, commit, &[outcome("fmt", Status::Running)]).unwrap();
        update_run(&repo, commit, &[outcome("fmt", Status::Pass)]).unwrap();
        let commits = runs(&repo).unwrap();
        assert_eq!(commits[0].runs.len(), 1);
        assert_eq!(
            commits[0].runs[0].results,
            vec![outcome("fmt", Status::Pass)]
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn empty_when_no_runs_recorded() {
        let repo = unique_repo();
        assert!(runs(&repo).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&repo);
    }

    // @relation(checks.outcomes, role=Verifies)
    #[test]
    fn round_trips_an_outcomes_duration_and_recording() {
        let repo = unique_repo();
        let commit = ObjectId::from_hex(b"0123456789012345678901234567890123456789").unwrap();
        let rich = outcome("fmt", Status::Pass);
        let rich = RunOutcome {
            duration_secs: Some(12),
            recording: Some("{\"version\": 2}\n[0.5, \"o\", \"hi\\r\\n\"]\n".to_owned()),
            exit_code: Some(0),
            ..rich
        };
        record(&repo, commit, std::slice::from_ref(&rich)).unwrap();
        let commits = runs(&repo).unwrap();
        assert_eq!(commits[0].runs[0].results, vec![rich]);
        let _ = std::fs::remove_dir_all(&repo);
    }

    // @relation(checks.outcomes, role=Verifies)
    #[test]
    fn displays_lowercase_status_words() {
        assert_eq!(Status::Queued.to_string(), "queued");
        assert_eq!(Status::Pass.to_string(), "pass");
        assert_eq!(Status::Skipped.to_string(), "skipped");
    }

    #[test]
    fn loads_the_on_disk_result_format() {
        // A fixture written as the real `status/<Variant>` subtree layout,
        // with `duration_secs`/`recording` omitted, must keep loading, with
        // the missing optional fields unset.
        let repo = unique_repo();
        let commit = ObjectId::from_hex(b"0123456789012345678901234567890123456789").unwrap();
        write_result_doc(&repo, "fmt", commit, "Pass");
        let commits = runs(&repo).unwrap();
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].commit, commit);
        assert_eq!(
            commits[0].runs[0].results,
            vec![outcome("fmt", Status::Pass)]
        );
        let _ = std::fs::remove_dir_all(&repo);
    }
}
