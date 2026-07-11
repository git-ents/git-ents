//! Integration coverage for `git ents comment` against a real local
//! composition root (`roots.local`) — adding a comment, then listing it
//! back (`model.comment`).

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "integration test"
)]

mod common;

use std::path::Path;
use std::process::Command;

use git_ents::commands::comment;
use git_ents::root::LocalRoot;

/// Seed `dir`'s working tree with `path` and commit it under a fixed test
/// identity — the content a comment anchors to, distinct from the signed
/// `refs/meta/*` mutation commits `common::Fixture`'s key produces.
fn commit_file(dir: &Path, path: &str, contents: &str) {
    std::fs::write(dir.join(path), contents).expect("write");
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["add", "-A"])
        .status()
        .expect("git add");
    assert!(status.success());
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-q",
            "-m",
            "seed",
        ])
        .status()
        .expect("git commit");
    assert!(status.success());
}

/// `git ents comment list` surfaces every recorded comment's id and body —
/// the only way to discover a comment's id before `show` can be run
/// against it (`model.comment`).
// @relation(roots.local, model.comment, scope=function, role=Verifies)
#[test]
fn list_returns_every_recorded_comment() {
    let fixture = common::Fixture::new(1);
    commit_file(fixture.path(), "file.txt", "line one\nline two\n");
    let root = LocalRoot::open(fixture.path()).expect("opens");

    let id = comment::add(
        &root,
        "file.txt",
        "looks off by one".to_owned(),
        None,
        "HEAD",
        Some(fixture.key_path.clone()),
    )
    .expect("adds");

    let listed = comment::list(&root).expect("lists");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].0, id);
    assert_eq!(listed[0].1.body, "looks off by one");
}
