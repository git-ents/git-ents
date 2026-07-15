//! Integration coverage for `git ents comment` against a real local
//! composition root (`roots.local`) — the phase-9 comment loop: a comment
//! anchored against a dirty working tree (`anchor.working-tree`) is listed
//! open by the machine-readable `comment list --worktree` form
//! (`lens.parity`), resolved through the CLI (`model.comment-state`), and
//! gone from the open listing afterwards.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "integration test"
)]

mod common;

use std::path::Path;
use std::process::Command;

use ents_forge::comment::{ListFilter, NewComment};
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

fn draft(body: &str) -> NewComment {
    NewComment {
        body: body.to_owned(),
        path: Some("file.txt".to_owned()),
        lines: None,
        rev: "HEAD".to_owned(),
        worktree: false,
        context: None,
        parent: None,
    }
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
        draft("looks off by one"),
        Some(fixture.key_path.clone()),
    )
    .expect("adds");

    let listed = comment::list(&root).expect("lists");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].0, id);
    assert_eq!(listed[0].1.body, "looks off by one");
    assert_eq!(listed[0].1.state, "open");
}

/// The phase-9 comment loop, CLI end: a comment anchored to a *dirty*
/// working tree is listed open by the machine-readable form with its
/// worktree projection, resolved, and gone from the open listing — an
/// agent needs nothing but this surface (`lens.parity`,
/// `anchor.working-tree`, `model.comment-state`).
// @relation(lens.parity, anchor.working-tree, model.comment-state, roots.local, scope=function, role=Verifies)
#[test]
fn the_comment_loop_runs_through_the_machine_readable_listing() {
    let fixture = common::Fixture::new(1);
    let contents: String = (1..=10).map(|n| format!("line {n}\n")).collect();
    commit_file(fixture.path(), "file.txt", &contents);
    // Dirty the working tree: the comment anchors to bytes HEAD never saw.
    let dirty = contents.replace("line 5\n", "line five\n");
    std::fs::write(fixture.path().join("file.txt"), &dirty).expect("write");

    let root = LocalRoot::open(fixture.path()).expect("opens");
    let mut new = draft("this new line looks wrong\n\nsecond paragraph");
    new.worktree = true;
    new.lines = Some("5".to_owned());
    new.context = Some("issues/42".to_owned());
    let id = comment::add(&root, new, Some(fixture.key_path.clone())).expect("adds");

    // Open, current against the working tree, machine-readable.
    let open = ListFilter {
        state: Some("open".to_owned()),
        context: None,
    };
    let (rows, _unreadable) = comment::list_projected(&root, true, &open).expect("lists");
    let rendered = comment::porcelain(&rows);
    let expected = format!(
        "{id} open current file.txt:5-5\ncontext issues/42\n\tthis new line looks wrong\n\t\n\tsecond paragraph\n"
    );
    assert_eq!(rendered, expected);

    // Resolve through the CLI surface; the open listing no longer shows
    // it, the unfiltered one shows it resolved.
    comment::set_state(&root, &id, true, Some(fixture.key_path.clone())).expect("resolves");
    let (rows, _unreadable) = comment::list_projected(&root, true, &open).expect("lists");
    assert!(rows.is_empty(), "a resolved comment is not open");
    let (all, _unreadable) =
        comment::list_projected(&root, true, &ListFilter::default()).expect("lists");
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].comment.state, "resolved");
}

/// Two records separate with exactly one blank line, and an unanchored
/// reply renders `-` for projection and location — the porcelain grammar
/// an agent parses (`lens.parity`).
// @relation(lens.parity, scope=function, role=Verifies)
#[test]
fn porcelain_separates_records_and_renders_unanchored_comments() {
    let fixture = common::Fixture::new(1);
    commit_file(fixture.path(), "file.txt", "line one\nline two\n");
    let root = LocalRoot::open(fixture.path()).expect("opens");

    let first = comment::add(&root, draft("root"), Some(fixture.key_path.clone())).expect("adds");
    let second = comment::reply(
        &root,
        &first,
        "reply".to_owned(),
        Some(fixture.key_path.clone()),
    )
    .expect("replies");

    let (rows, _unreadable) =
        comment::list_projected(&root, false, &ListFilter::default()).expect("lists");
    let rendered = comment::porcelain(&rows);
    let records: Vec<&str> = rendered.split("\n\n").collect();
    assert_eq!(records.len(), 2);
    let root_record = records
        .iter()
        .find(|r| r.starts_with(&first))
        .expect("root listed");
    assert!(root_record.contains(&format!("{first} open current file.txt\n")));
    let reply_record = records
        .iter()
        .find(|r| r.starts_with(&second))
        .expect("reply listed");
    assert!(reply_record.contains(&format!("{second} open - -\n")));
    assert!(reply_record.contains(&format!("parent {first}\n")));
}
