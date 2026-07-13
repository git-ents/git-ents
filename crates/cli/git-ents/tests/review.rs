//! Integration coverage for `git ents review` against a real local
//! composition root (`roots.local`): reviewing a commit writes both refs
//! `model.review` requires — the entity ref and its retention pin
//! (`model.review-pin`), the pin's parents including the reviewed commit
//! and its tree the empty tree — and a review's discussion thread
//! surfaces comments naming it as their context (`model.comment-context`).

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "integration test"
)]

mod common;

use std::path::Path;
use std::process::Command;

use ents_forge::comment::NewComment;
use ents_forge::review::NewReview;
use git_ents::commands::{comment, review};
use git_ents::root::LocalRoot;
use gix_object::{CommitRef, Find, Write as _};
use gix_ref_store::RefStoreRead as _;

/// Seed `dir`'s working tree with `path` and commit it under a fixed test
/// identity, returning the new commit's id.
fn commit_file(dir: &Path, path: &str, contents: &str) -> gix_hash::ObjectId {
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
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("rev-parse");
    let hex = String::from_utf8(output.stdout).expect("utf8");
    hex.trim().parse().expect("valid oid")
}

/// `model.review`, `model.review-pin`: `git ents review new` writes both
/// the review's own entity ref and its retention pin, and the pin's tip
/// commit is a merge-shaped, empty-tree commit whose parents include the
/// reviewed commit — the reachability edge `model.review-pin` requires.
// @relation(model.review, model.review-pin, roots.local, scope=function, role=Verifies)
#[test]
fn review_new_writes_both_refs_with_the_pin_parented_on_the_reviewed_commit() {
    let fixture = common::Fixture::new(1);
    let reviewed = commit_file(fixture.path(), "file.txt", "line one\n");
    let root = LocalRoot::open(fixture.path()).expect("opens");

    let new = NewReview {
        target: "HEAD".to_owned(),
        verdict: "approve".to_owned(),
        body: "looks good".to_owned(),
    };
    let id = review::new(&root, new, Some(fixture.key_path.clone())).expect("reviews");

    // The entity ref exists and reads back verdict, body, and the
    // reviewed commit as a plain data field — no pin read required.
    let (found, _thread) = review::show(&root, &id).expect("shows");
    assert_eq!(found.verdict, "approve");
    assert_eq!(found.body, "looks good");
    assert_eq!(found.commit(), reviewed);

    // The pin ref exists; its tip's parents include the reviewed commit,
    // and its tree is the empty tree — the sole exception to
    // `meta-ref.namespace`'s tree-is-the-entity shape.
    let pin_ref = ents_model::namespace::review_pin_ref(&id).expect("valid");
    let pin_tip = root
        .refs
        .get(pin_ref.as_ref())
        .expect("reads")
        .expect("pin ref exists");
    let mut buf = Vec::new();
    let data = root
        .objects
        .try_find(&pin_tip, &mut buf)
        .expect("reads")
        .expect("pin commit exists");
    let commit = CommitRef::from_bytes(data.data, pin_tip.kind()).expect("parses");
    assert!(
        commit.parents().any(|parent| parent == reviewed),
        "pin's parents must include the reviewed commit"
    );
    let empty_tree = root
        .objects
        .write(&gix_object::Tree { entries: vec![] })
        .expect("writes empty tree");
    assert_eq!(commit.tree(), empty_tree);
}

/// `model.comment-context`, `model.review`: a comment naming
/// `reviews/<id>` as its context surfaces in `git ents review show`'s
/// thread — the review itself stores no list of its comments.
// @relation(model.review, model.comment-context, roots.local, scope=function, role=Verifies)
#[test]
fn review_show_surfaces_a_context_comment() {
    let fixture = common::Fixture::new(1);
    commit_file(fixture.path(), "file.txt", "line one\n");
    let root = LocalRoot::open(fixture.path()).expect("opens");

    let new = NewReview {
        target: "HEAD".to_owned(),
        verdict: "request-changes".to_owned(),
        body: "one nit".to_owned(),
    };
    let id = review::new(&root, new, Some(fixture.key_path.clone())).expect("reviews");

    let draft = NewComment {
        body: "please rename this".to_owned(),
        path: None,
        lines: None,
        rev: "HEAD".to_owned(),
        worktree: false,
        context: Some(format!("reviews/{id}")),
        parent: None,
    };
    comment::add(&root, draft, Some(fixture.key_path.clone())).expect("comments");

    let (_review, thread) = review::show(&root, &id).expect("shows");
    assert_eq!(thread.len(), 1);
    assert_eq!(thread[0].1.body, "please rename this");
}

/// `git ents review list [--target rev]`: filtering by target keeps only
/// reviews of that commit.
// @relation(model.review, roots.local, scope=function, role=Verifies)
#[test]
fn review_list_filters_by_target() {
    let fixture = common::Fixture::new(1);
    let first = commit_file(fixture.path(), "file.txt", "line one\n");
    let second = commit_file(fixture.path(), "file.txt", "line one\nline two\n");
    let root = LocalRoot::open(fixture.path()).expect("opens");

    let review_of_first = NewReview {
        target: first.to_string(),
        verdict: "approve".to_owned(),
        body: String::new(),
    };
    let first_id =
        review::new(&root, review_of_first, Some(fixture.key_path.clone())).expect("reviews");
    let review_of_second = NewReview {
        target: second.to_string(),
        verdict: "approve".to_owned(),
        body: String::new(),
    };
    review::new(&root, review_of_second, Some(fixture.key_path.clone())).expect("reviews");

    let all = review::list(&root, None).expect("lists");
    assert_eq!(all.len(), 2);

    let filtered = review::list(&root, Some(first.to_string())).expect("lists");
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].0, first_id);
}
