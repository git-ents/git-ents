//! The configured checks, sourced from the `refs/meta/checks` ref.
//!
//! A check is anything a server runs against a push — CI, CD, linting,
//! versioning gates, and so on. Their definitions live in exactly one place:
//! the `refs/meta/checks` ref, whose tree is a scalar-keyed map from each
//! check name to the [`CheckBody`] that runs it. The document is read and
//! written through [`git_store`], so the check set is a typed value that
//! lives in git — versioned, auditable, and itself pushable. Keeping it on a
//! meta ref rather than in the worktree means an untrusted branch cannot
//! rewrite the checks that gate it.
//!
//! # Migration note
//!
//! `checks/<name>` and `results/<name>` moved from bare blobs to subtrees
//! (`CheckBody`/[`Outcome`]) so a run's outcome can carry more than one field
//! (a duration, a log URL), and a run's [`Status`] moved from a bare string to
//! a closed enum. [`CheckBody::command`] then moved from a required blob to an
//! `Option` subtree when checks gained `image` and `depends`, so a composite
//! check can exist without a command. Each is an incompatible format change:
//! data written in a prior layout no longer loads and must be re-recorded.
//! Acceptable pre-1.0 (see the format compatibility rules in `git_store`'s
//! module docs).

use std::path::Path;

use facet::Facet;
use gix::ObjectId;

use crate::component;

/// The ref whose tree holds the configured check set.
pub const CHECKS_REF: &str = "refs/meta/checks";

/// A configured check's on-disk body. The map key (its name) is the check's
/// identity, so it is not duplicated inside the body. `pub` only because it
/// is [`component::MapDocument::Body`] for [`Check`]; nothing outside this
/// module constructs one directly.
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct CheckBody {
    /// The shell command run for the check (e.g. `cargo fmt --check`), or
    /// `None` for a composite check that only aggregates its `depends`.
    command: Option<String>,
    /// The sandbox image the command runs in; `None` uses the default.
    image: Option<String>,
    /// Names of sibling checks that must pass before this one runs. Stored as
    /// `None` when empty so an independent check stays a minimal tree.
    depends: Option<Vec<String>>,
}

/// One configured check, assembled from its map key and [`CheckBody`] at load.
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct Check {
    /// The name it is stored under.
    pub name: String,
    /// The shell command run for the check (e.g. `cargo fmt --check`), or
    /// `None` for a composite check that only aggregates its dependencies.
    pub command: Option<String>,
    /// The sandbox image the command runs in; `None` uses the default.
    pub image: Option<String>,
    /// Names of sibling checks that must pass before this one runs.
    pub depends: Vec<String>,
}

impl component::MapDocument for Check {
    const REF: &'static str = CHECKS_REF;
    type Body = CheckBody;

    fn compose(name: String, body: CheckBody) -> Self {
        Check {
            name,
            command: body.command,
            image: body.image,
            depends: body.depends.unwrap_or_default(),
        }
    }

    fn decompose(&self) -> (&str, CheckBody) {
        (
            &self.name,
            CheckBody {
                command: self.command.clone(),
                image: self.image.clone(),
                depends: if self.depends.is_empty() {
                    None
                } else {
                    Some(self.depends.clone())
                },
            },
        )
    }
}

impl component::Component for Check {
    const NOUN: &'static str = "check";
    const PLURAL: &'static str = "checks";
}

/// Load the configured checks recorded at [`CHECKS_REF`] in `repo`.
///
/// An absent ref yields an empty set, as on a server whose check set has not
/// been pushed yet. A present but unreadable ref is an error so callers can
/// distinguish corruption from "no checks configured".
pub fn load(repo: &Path) -> Result<Vec<Check>, git_store::Error> {
    component::load_map(&git_store::Store::open(repo)?)
}

/// Write `checks` to [`CHECKS_REF`] in `repo`, replacing any existing set as a
/// new commit.
pub fn store(repo: &Path, checks: &[Check]) -> Result<(), git_store::Error> {
    component::store_map(&git_store::Store::open(repo)?, checks, "Update checks")
}

/// Validate `checks` as a static dependency graph and return them in an order
/// that runs every check after its dependencies — Kahn's topological sort,
/// with ties broken by name so the order is deterministic.
///
/// Rejected here, at write time, so the worker only ever walks a fixed order:
/// a `depends` entry naming no configured check, a duplicate or self edge, a
/// check with neither a command nor dependencies, and any dependency cycle
/// (reported with its member names). A check that sets an `image` is also
/// rejected until the Sprite sandbox can honor one — the field exists in the
/// format now so supporting it later is not a data migration.
pub fn order(checks: &[Check]) -> Result<Vec<&Check>, String> {
    let mut by_name: std::collections::BTreeMap<&str, &Check> = std::collections::BTreeMap::new();
    for check in checks {
        if by_name.insert(check.name.as_str(), check).is_some() {
            return Err(format!("check {} is defined twice", check.name));
        }
    }
    let mut blocking: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for check in checks {
        if check.command.is_none() && check.depends.is_empty() {
            return Err(format!(
                "check {} has neither a command nor dependencies",
                check.name
            ));
        }
        if check.image.is_some() {
            return Err(format!(
                "check {} sets an image, which the checks sandbox does not support yet",
                check.name
            ));
        }
        let mut seen = std::collections::BTreeSet::new();
        for dep in &check.depends {
            if !by_name.contains_key(dep.as_str()) {
                return Err(format!(
                    "check {} depends on unknown check {dep}",
                    check.name
                ));
            }
            if dep == &check.name {
                return Err(format!("check {} depends on itself", check.name));
            }
            if !seen.insert(dep.as_str()) {
                return Err(format!("check {} lists dependency {dep} twice", check.name));
            }
        }
        blocking.insert(check.name.as_str(), check.depends.len());
    }

    let mut ordered = Vec::with_capacity(checks.len());
    while ordered.len() < checks.len() {
        let ready: Vec<&str> = blocking
            .iter()
            .filter_map(|(name, blockers)| (*blockers == 0).then_some(*name))
            .collect();
        if ready.is_empty() {
            let cycle: Vec<&str> = blocking.keys().copied().collect();
            return Err(format!(
                "check dependencies form a cycle: {}",
                cycle.join(", ")
            ));
        }
        for name in ready {
            let _ready = blocking.remove(name);
            if let Some(check) = by_name.get(name) {
                ordered.push(*check);
            }
            for (blocked, blockers) in blocking.iter_mut() {
                if let Some(check) = by_name.get(blocked)
                    && check.depends.iter().any(|dep| dep == name)
                {
                    *blockers = blockers.saturating_sub(1);
                }
            }
        }
    }
    Ok(ordered)
}

/// The namespace under which a commit's check runs are recorded: one ref,
/// `refs/meta/runs/<commit>`, per checked commit, holding the *log* of every
/// run against it. Definitions live on [`CHECKS_REF`]; this is their history.
pub const RUNS_NS: &str = "refs/meta/runs";

/// A check run's status, progressing `Queued` → `Running` → a terminal
/// outcome. Closed set — the only values a run legitimately takes, in place
/// of a `String` that every caller had to trust held one of five values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Facet)]
#[repr(u8)]
pub enum Status {
    /// Enqueued by `post-receive`, not yet picked up by the worker.
    Queued,
    /// The worker has started this run.
    Running,
    /// The check exited successfully.
    Pass,
    /// The check exited with a failure.
    Fail,
    /// An infrastructure failure (an unreachable sandbox, a timeout) kept the
    /// check from completing.
    Error,
    /// The check never ran because a dependency did not pass.
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

/// One check's on-disk outcome. The map key (the check's name) is not
/// duplicated inside it. Optional fields absent from an older record load as
/// unset, so a run recorded before a field existed still loads.
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
struct Outcome {
    /// `queued`, `running`, then `pass`, `fail`, or `error`.
    status: Status,
    /// How long the check took to run, when known.
    duration_secs: Option<u64>,
    /// The check's terminal session, captured as asciicast v2 (JSONL) text,
    /// when the runner recorded one.
    recording: Option<String>,
    /// The command's process exit code, when the check ran to completion
    /// rather than erroring out before or during execution (an unreachable
    /// sandbox, a timeout).
    exit_code: Option<i32>,
}

/// One check's outcome within a [`Run`], assembled from its map key and
/// [`Outcome`] at load.
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct RunOutcome {
    /// The check's name (its `checks/<name>` in [`CHECKS_REF`]).
    pub name: String,
    /// The outcome recorded for it as a run progresses.
    pub status: Status,
    /// How long the check took to run, when known.
    pub duration_secs: Option<u64>,
    /// The check's terminal session, captured as asciicast v2 (JSONL) text,
    /// when the runner recorded one.
    pub recording: Option<String>,
    /// The command's process exit code, when the check ran to completion
    /// rather than erroring out before or during execution (an unreachable
    /// sandbox, a timeout).
    pub exit_code: Option<i32>,
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
    pub commit: ObjectId,
    /// Every run against it, newest first.
    pub runs: Vec<Run>,
}

/// Record a run of `outcomes` for `commit` in `repo`, as a new commit on
/// `refs/meta/runs/<commit>`, parented on the prior run so the ref's commit
/// chain is the run history. The commit's date is the run time.
pub fn record(
    repo: &Path,
    commit: ObjectId,
    outcomes: &[RunOutcome],
) -> Result<(), git_store::Error> {
    let store = git_store::Store::open(repo)?;
    store.store_map(
        &format!("{RUNS_NS}/{commit}"),
        outcomes,
        outcome_split,
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
    commit: ObjectId,
    outcomes: &[RunOutcome],
) -> Result<(), git_store::Error> {
    let refname = format!("{RUNS_NS}/{commit}");
    let doc: std::collections::BTreeMap<String, Outcome> =
        outcomes.iter().map(outcome_split).collect();
    git_store::Store::open(repo)?.amend(&refname, &doc, "Record check run")
}

/// List the recorded runs per commit in `repo`, newest commit first. Each
/// commit's runs are the ref's commit chain, newest first, with the run time
/// taken from each commit's date.
///
/// A ref whose last segment is not a valid hex object id cannot have been
/// written by [`record`]/[`update_run`], so it is skipped rather than
/// surfaced as an error — the same tolerance [`runs`] already gives a foreign
/// ref under [`RUNS_NS`].
pub fn runs(repo: &Path) -> Result<Vec<CommitRuns>, git_store::Error> {
    let store = git_store::Store::open(repo)?;
    let prefix = format!("{RUNS_NS}/");
    let mut commits = Vec::new();
    for refname in store.list(&prefix)? {
        let Some(commit) = refname
            .strip_prefix(&prefix)
            .and_then(|hex| ObjectId::from_hex(hex.as_bytes()).ok())
        else {
            continue;
        };
        let runs = store
            .history::<std::collections::BTreeMap<String, Outcome>>(&refname)?
            .into_iter()
            .map(|(at, doc)| Run {
                at,
                results: doc
                    .into_iter()
                    .map(|(name, outcome)| assemble_outcome(name, outcome))
                    .collect(),
            })
            .collect();
        commits.push(CommitRuns { commit, runs });
    }
    Ok(commits)
}

/// Split a public [`RunOutcome`] into its map key and on-disk [`Outcome`].
fn outcome_split(outcome: &RunOutcome) -> (String, Outcome) {
    (
        outcome.name.clone(),
        Outcome {
            status: outcome.status,
            duration_secs: outcome.duration_secs,
            recording: outcome.recording.clone(),
            exit_code: outcome.exit_code,
        },
    )
}

/// Assemble a public [`RunOutcome`] from its map key and on-disk [`Outcome`].
fn assemble_outcome(name: String, outcome: Outcome) -> RunOutcome {
    RunOutcome {
        name,
        status: outcome.status,
        duration_secs: outcome.duration_secs,
        recording: outcome.recording,
        exit_code: outcome.exit_code,
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
            command: Some(command.to_owned()),
            image: None,
            depends: Vec::new(),
        }
    }

    fn composite(name: &str, depends: &[&str]) -> Check {
        Check {
            name: name.to_owned(),
            command: None,
            image: None,
            depends: depends.iter().map(|dep| (*dep).to_owned()).collect(),
        }
    }

    fn dependent(name: &str, command: &str, depends: &[&str]) -> Check {
        Check {
            depends: depends.iter().map(|dep| (*dep).to_owned()).collect(),
            ..check(name, command)
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
        // A fixture written as the real `checks/<name>/command/some` subtree
        // layout (the `Option`-wrapped command, with `image`/`depends` omitted
        // entirely) must keep loading, with the missing optional fields unset —
        // guarding the checks document's shape against an incompatible change
        // to data already on a ref.
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
        // A fixture written as the real `results/<name>/status/<Variant>`
        // subtree layout, with `duration_secs`/`recording` omitted, must keep
        // loading, with the missing optional fields unset.
        let repo = unique_repo();
        let commit = ObjectId::from_hex(b"0123456789012345678901234567890123456789").unwrap();
        write_runs_doc(
            &repo,
            &format!("{RUNS_NS}/{commit}"),
            &[("fmt", "Pass"), ("test", "Fail")],
        );
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

    fn outcome(name: &str, status: Status) -> RunOutcome {
        RunOutcome {
            name: name.to_owned(),
            status,
            duration_secs: None,
            recording: None,
            exit_code: None,
        }
    }

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

    #[test]
    fn recording_a_commit_again_appends_a_run() {
        let repo = unique_repo();
        let commit = ObjectId::from_hex(b"0123456789012345678901234567890123456789").unwrap();
        record(&repo, commit, &[outcome("fmt", Status::Fail)]).unwrap();
        record(&repo, commit, &[outcome("fmt", Status::Pass)]).unwrap();
        let commits = runs(&repo).unwrap();
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].runs.len(), 2);
        // Newest first: the second run (pass) leads, the first (fail) follows.
        assert_eq!(
            commits[0].runs[0].results,
            vec![outcome("fmt", Status::Pass)]
        );
        assert_eq!(
            commits[0].runs[1].results,
            vec![outcome("fmt", Status::Fail)]
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn empty_when_no_runs_recorded() {
        let repo = unique_repo();
        assert!(runs(&repo).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn round_trips_an_outcomes_duration_and_recording() {
        let repo = unique_repo();
        let commit = ObjectId::from_hex(b"0123456789012345678901234567890123456789").unwrap();
        let rich = RunOutcome {
            name: "fmt".to_owned(),
            status: Status::Pass,
            duration_secs: Some(12),
            recording: Some("{\"version\": 2}\n[0.5, \"o\", \"hi\\r\\n\"]\n".to_owned()),
            exit_code: Some(0),
        };
        record(&repo, commit, std::slice::from_ref(&rich)).unwrap();
        let commits = runs(&repo).unwrap();
        assert_eq!(commits[0].runs[0].results, vec![rich]);
        let _ = std::fs::remove_dir_all(&repo);
    }

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
    fn displays_lowercase_status_words() {
        assert_eq!(Status::Queued.to_string(), "queued");
        assert_eq!(Status::Pass.to_string(), "pass");
        assert_eq!(Status::Skipped.to_string(), "skipped");
    }

    #[test]
    fn store_then_load_round_trips_image_and_depends() {
        let repo = unique_repo();
        let written = vec![
            Check {
                image: Some("rust:1.88".to_owned()),
                ..check("fmt", "cargo fmt --check")
            },
            dependent("test", "cargo nextest run", &["fmt"]),
            composite("ci", &["fmt", "test"]),
        ];
        store(&repo, &written).unwrap();
        let mut loaded = load(&repo).unwrap();
        loaded.sort_by(|a, b| a.name.cmp(&b.name));
        let mut expected = written;
        expected.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(loaded, expected);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn order_runs_dependencies_first() {
        let checks = vec![
            composite("ci", &["test", "fmt"]),
            dependent("test", "cargo nextest run", &["fmt"]),
            check("fmt", "cargo fmt --check"),
        ];
        let names: Vec<&str> = order(&checks)
            .unwrap()
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(names, vec!["fmt", "test", "ci"]);
    }

    #[test]
    fn order_rejects_a_cycle() {
        let checks = vec![
            dependent("a", "true", &["b"]),
            dependent("b", "true", &["a"]),
            check("fmt", "cargo fmt --check"),
        ];
        let err = order(&checks).unwrap_err();
        assert!(err.contains("cycle"), "unexpected error: {err}");
        assert!(err.contains('a') && err.contains('b'));
    }

    #[test]
    fn order_rejects_an_unknown_dependency() {
        let checks = vec![dependent("test", "cargo nextest run", &["fmt"])];
        let err = order(&checks).unwrap_err();
        assert!(err.contains("unknown check fmt"), "unexpected error: {err}");
    }

    #[test]
    fn order_rejects_self_and_duplicate_edges() {
        let selfish = vec![dependent("a", "true", &["a"])];
        assert!(order(&selfish).unwrap_err().contains("itself"));
        let doubled = vec![
            check("fmt", "true"),
            dependent("a", "true", &["fmt", "fmt"]),
        ];
        assert!(order(&doubled).unwrap_err().contains("twice"));
    }

    #[test]
    fn order_rejects_an_empty_check() {
        let checks = vec![composite("hollow", &[])];
        let err = order(&checks).unwrap_err();
        assert!(
            err.contains("neither a command nor dependencies"),
            "unexpected error: {err}"
        );
    }
}
