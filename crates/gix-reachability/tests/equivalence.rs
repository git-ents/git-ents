//! Conformance-style equivalence for the accelerated walk
//! (`docs/scale-out.adoc`, "Reachability": "absence or staleness degrades
//! speed, never answers"): over several generated DAG shapes, the
//! commit-graph-accelerated walk must return exactly the same reachable set
//! as the plain one, and a stale cached [`gix_reachability::reachable_set::
//! ReachableSetArtifact`] must still yield a correct answer once new commits
//! have landed past the frontier it was built from.
//!
//! Runs against real `odb-files`/`refstore-files` repositories built with
//! the `git` CLI (mirroring `git-protocol`'s own test fixtures) rather than
//! synthetic in-memory commits, so the DAG shapes exercise real tree/blob
//! objects too, not just the commit-parent skeleton.

#![allow(clippy::unwrap_used, reason = "test fixture")]

use std::path::Path;
use std::process::Command;

use gix_hash::ObjectId;
use gix_reachability::commitgraph::CommitGraph;
use gix_reachability::engine::{self, ArtifactBundle};
use gix_reachability::reachable_set::ReachableSetArtifact;
use gix_reachability::walk::{self, StoreSource};

fn bare_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let status = Command::new("git")
        .args(["init", "-q", "--bare", "-b", "main"])
        .arg(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    dir
}

fn run(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .unwrap();
    assert!(status.success());
}

fn ref_exists(bare: &Path, branch: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(bare)
        .args([
            "rev-parse",
            "--verify",
            "-q",
            &format!("refs/heads/{branch}"),
        ])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn rev_parse_head(work: &Path) -> ObjectId {
    let hex = String::from_utf8(
        Command::new("git")
            .arg("-C")
            .arg(work)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    ObjectId::from_hex(hex.trim().as_bytes()).unwrap()
}

/// Commit `content` in `file_name` onto `branch` in `bare`, basing the new
/// commit on `branch`'s (or, if `branch` doesn't exist yet, `main`'s)
/// current tip so a series of calls with the same `branch` builds a linear
/// chain. Returns the new commit's id.
fn commit_onto(bare: &Path, branch: &str, file_name: &str, content: &str) -> ObjectId {
    let work = tempfile::tempdir().unwrap();
    run(work.path(), &["init", "-q", "-b", branch]);
    run(work.path(), &["config", "user.email", "test@example.com"]);
    run(work.path(), &["config", "user.name", "test"]);

    let base = if ref_exists(bare, branch) {
        Some(branch)
    } else if ref_exists(bare, "main") {
        Some("main")
    } else {
        None
    };
    if let Some(base) = base {
        run(work.path(), &["fetch", "-q", bare.to_str().unwrap(), base]);
        run(work.path(), &["reset", "-q", "--hard", "FETCH_HEAD"]);
    }

    std::fs::write(work.path().join(file_name), content).unwrap();
    run(work.path(), &["add", "-A"]);
    run(work.path(), &["commit", "-q", "-m", "commit"]);
    let commit = rev_parse_head(work.path());
    run(
        work.path(),
        &[
            "push",
            bare.to_str().unwrap(),
            &format!("HEAD:refs/heads/{branch}"),
        ],
    );
    commit
}

/// Merge `from` into `into` in `bare` with a real merge commit (two
/// parents), and push the result back onto `into`. Returns the merge
/// commit's id.
fn merge(bare: &Path, into: &str, from: &str) -> ObjectId {
    let work = tempfile::tempdir().unwrap();
    run(work.path(), &["init", "-q", "-b", into]);
    run(work.path(), &["config", "user.email", "test@example.com"]);
    run(work.path(), &["config", "user.name", "test"]);
    run(work.path(), &["fetch", "-q", bare.to_str().unwrap(), into]);
    run(work.path(), &["reset", "-q", "--hard", "FETCH_HEAD"]);
    run(
        work.path(),
        &[
            "fetch",
            "-q",
            bare.to_str().unwrap(),
            &format!("{from}:{from}"),
        ],
    );
    run(
        work.path(),
        &["merge", "-q", "--no-ff", "-m", "merge", from],
    );
    let commit = rev_parse_head(work.path());
    run(
        work.path(),
        &[
            "push",
            bare.to_str().unwrap(),
            &format!("HEAD:refs/heads/{into}"),
        ],
    );
    commit
}

/// Assert the commit-graph-accelerated walk agrees with the plain walk from
/// `tips`, both directly ([`walk::reachable_with_graph`]) and through
/// [`engine::accelerated_reachable`] with no cached reachable-set (so only
/// the commit-graph acceleration is in play).
fn assert_accelerated_matches_slow(objects: &odb_files::OdbFiles, tips: &[ObjectId]) {
    let source = StoreSource::new(objects);

    let slow = walk::reachable(tips.iter().copied(), &source, |_id| false, false).unwrap();

    let graph = CommitGraph::build(tips.iter().copied(), &source).unwrap();
    let accelerated = walk::reachable_with_graph(
        tips.iter().copied(),
        &source,
        |_id| false,
        false,
        Some(&graph),
    )
    .unwrap();
    assert_eq!(
        accelerated, slow,
        "graph-accelerated walk disagreed with the slow one"
    );

    let bundle = ArtifactBundle {
        commit_graph: Some(graph),
        reachable_set: None,
    };
    let via_engine =
        engine::accelerated_reachable(tips.iter().copied(), &source, |_id| false, false, &bundle)
            .unwrap();
    assert_eq!(
        via_engine, slow,
        "engine entry point disagreed with the slow walk"
    );
}

#[test]
fn linear_chain() {
    let bare = bare_repo();
    commit_onto(bare.path(), "main", "a", "1");
    commit_onto(bare.path(), "main", "a", "2");
    let tip = commit_onto(bare.path(), "main", "a", "3");

    let objects = odb_files::OdbFiles::open(bare.path()).unwrap();
    assert_accelerated_matches_slow(&objects, &[tip]);
}

#[test]
fn diamond_merge() {
    let bare = bare_repo();
    commit_onto(bare.path(), "main", "base", "0");
    commit_onto(bare.path(), "left", "left", "l");
    commit_onto(bare.path(), "right", "right", "r");
    merge(bare.path(), "main", "left");
    let tip = merge(bare.path(), "main", "right");

    let objects = odb_files::OdbFiles::open(bare.path()).unwrap();
    assert_accelerated_matches_slow(&objects, &[tip]);
}

#[test]
fn disconnected_roots_as_separate_tips() {
    let bare = bare_repo();
    let tip_a = commit_onto(bare.path(), "a", "a", "1");
    let tip_b = commit_onto(bare.path(), "b", "b", "1");

    let objects = odb_files::OdbFiles::open(bare.path()).unwrap();
    assert_accelerated_matches_slow(&objects, &[tip_a, tip_b]);
}

#[test]
fn a_stale_reachable_set_snapshot_still_yields_a_correct_answer_after_new_commits_land() {
    let bare = bare_repo();
    let old_tip = commit_onto(bare.path(), "main", "a", "1");

    let objects = odb_files::OdbFiles::open(bare.path()).unwrap();
    let source = StoreSource::new(&objects);
    let stale = ReachableSetArtifact::build([old_tip], &source).unwrap();

    let new_tip = commit_onto(bare.path(), "main", "a", "2");
    let bundle = ArtifactBundle {
        commit_graph: None,
        reachable_set: Some(stale),
    };

    let accelerated =
        engine::accelerated_reachable([new_tip], &source, |_id| false, false, &bundle).unwrap();
    let slow = walk::reachable([new_tip], &source, |_id| false, false).unwrap();

    assert_eq!(
        accelerated, slow,
        "a stale frontier must fall back to a full walk, not a wrong answer"
    );
    // The new tip's own commit must be present — a bug that just returned
    // the stale cached set unconditionally would have missed it.
    assert!(accelerated.contains(&new_tip));
}

#[test]
fn commit_graph_missing_a_new_commit_still_degrades_correctly() {
    let bare = bare_repo();
    let old_tip = commit_onto(bare.path(), "main", "a", "1");

    let objects = odb_files::OdbFiles::open(bare.path()).unwrap();
    let source = StoreSource::new(&objects);
    // A graph built before `new_tip` exists: `entry()` will return `None`
    // for it, so the walk must fall back to an ordinary object-store read
    // for exactly that commit.
    let stale_graph = CommitGraph::build([old_tip], &source).unwrap();

    let new_tip = commit_onto(bare.path(), "main", "a", "2");
    let accelerated =
        walk::reachable_with_graph([new_tip], &source, |_id| false, false, Some(&stale_graph))
            .unwrap();
    let slow = walk::reachable([new_tip], &source, |_id| false, false).unwrap();
    assert_eq!(accelerated, slow);
}
