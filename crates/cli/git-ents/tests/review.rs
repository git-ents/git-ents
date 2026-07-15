//! Integration coverage for `git ents review` against a real local
//! composition root (`roots.local`): reviewing a commit writes both refs
//! `model.review` requires — the entity ref and its retention pin
//! (`model.review-pin`), the pin's parents including the reviewed commit
//! and its tree the empty tree — a review's discussion thread surfaces
//! comments naming it as their context (`model.comment-context`), and
//! re-reviewing a descendant advances the same composite-keyed ref
//! fast-forward rather than minting a new one (`model.review-pin`).

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
use ents_forge::review::Verdict;
use git_ents::commands::{comment, members, review};
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
/// the review's own entity ref (keyed `reviews/<target>/<member>`) and its
/// retention pin, and the pin's tip commit is a merge-shaped, empty-tree
/// commit whose parents include the reviewed commit — the reachability
/// edge `model.review-pin` requires.
// @relation(model.review, model.review-pin, meta-ref.identity-binding, roots.local, scope=function, role=Verifies)
#[test]
fn review_new_writes_both_refs_with_the_pin_parented_on_the_reviewed_commit() {
    let fixture = common::Fixture::new(1);
    let reviewed = commit_file(fixture.path(), "file.txt", "line one\n");
    let root = LocalRoot::open(fixture.path()).expect("opens");
    members::add(&root, "reviewer", None, Some(fixture.key_path.clone())).expect("enrolls");

    let new = NewReview {
        target: "HEAD".to_owned(),
        verdict: Verdict::Approve,
        body: "looks good".to_owned(),
    };
    let target = review::new(&root, new, Some(fixture.key_path.clone())).expect("reviews");

    // The entity ref exists and reads back verdict, body, and the
    // reviewed commit as a plain data field — no pin read required.
    let (found, _thread) = review::show(&root, &target, "reviewer").expect("shows");
    assert_eq!(found.verdict, Verdict::Approve);
    assert_eq!(found.body, "looks good");
    assert_eq!(found.target(), reviewed);

    // The pin ref exists; its tip's parents include the reviewed commit,
    // and its tree is the empty tree — the sole exception to
    // `meta-ref.namespace`'s tree-is-the-entity shape.
    let pin_ref =
        ents_model::namespace::review_pin_ref(&target, &ents_model::MemberId::new("reviewer"))
            .expect("valid");
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
/// `reviews/<target>/<member>` as its context surfaces in `git ents review
/// show`'s thread — the review itself stores no list of its comments.
// @relation(model.review, model.comment-context, roots.local, scope=function, role=Verifies)
#[test]
fn review_show_surfaces_a_context_comment() {
    let fixture = common::Fixture::new(1);
    commit_file(fixture.path(), "file.txt", "line one\n");
    let root = LocalRoot::open(fixture.path()).expect("opens");
    members::add(&root, "reviewer", None, Some(fixture.key_path.clone())).expect("enrolls");

    let new = NewReview {
        target: "HEAD".to_owned(),
        verdict: Verdict::RequestChanges,
        body: "one nit".to_owned(),
    };
    let target = review::new(&root, new, Some(fixture.key_path.clone())).expect("reviews");

    let draft = NewComment {
        body: "please rename this".to_owned(),
        path: None,
        lines: None,
        rev: "HEAD".to_owned(),
        worktree: false,
        context: Some(format!("reviews/{target}/reviewer")),
        parent: None,
    };
    comment::add(&root, draft, Some(fixture.key_path.clone())).expect("comments");

    let (_review, thread) = review::show(&root, &target, "reviewer").expect("shows");
    assert_eq!(thread.len(), 1);
    assert_eq!(thread[0].1.body, "please rename this");
}

/// `git ents review list [--target rev]`: filtering by target keeps only
/// reviews of that commit — exercised across two different reviewers
/// (rather than one reviewer reviewing two commits) since, per
/// `model.review-pin`, one member reviewing a descendant commit advances
/// their existing thread rather than opening a second one; two distinct
/// review entities need two distinct reviewers.
// @relation(model.review, roots.local, scope=function, role=Verifies)
#[test]
fn review_list_filters_by_target() {
    let fixture = common::Fixture::new(1);
    let other_key = fixture.path().join(".id_ed25519_bob");
    common::write_key(&other_key, 2);
    let first = commit_file(fixture.path(), "file.txt", "line one\n");
    let second = commit_file(fixture.path(), "file.txt", "line one\nline two\n");
    let root = LocalRoot::open(fixture.path()).expect("opens");
    members::add(&root, "alice", None, Some(fixture.key_path.clone())).expect("enrolls alice");
    members::add(&root, "bob", None, Some(other_key.clone())).expect("enrolls bob");

    let review_of_first = NewReview {
        target: first.to_string(),
        verdict: Verdict::Approve,
        body: String::new(),
    };
    let first_target =
        review::new(&root, review_of_first, Some(fixture.key_path.clone())).expect("reviews");
    let review_of_second = NewReview {
        target: second.to_string(),
        verdict: Verdict::Approve,
        body: String::new(),
    };
    review::new(&root, review_of_second, Some(other_key)).expect("reviews");

    let all = review::list(&root, None).expect("lists");
    assert_eq!(all.len(), 2);

    let filtered = review::list(&root, Some(first.to_string())).expect("lists");
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].0.0, first_target);
    assert_eq!(filtered[0].0.1, ents_model::MemberId::new("alice"));
}

/// `model.review-pin`: re-reviewing a descendant of a commit this member
/// already reviewed advances the *same* two refs fast-forward — the
/// composite key stays anchored at the original genesis target, and
/// [`ents_forge::review::Review::target`] moves to the newly reviewed
/// commit — rather than minting a second, unrelated review.
// @relation(model.review, model.review-pin, meta-ref.identity-binding, roots.local, scope=function, role=Verifies)
#[test]
fn re_reviewing_a_descendant_advances_the_same_ref_fast_forward() {
    let fixture = common::Fixture::new(1);
    let first = commit_file(fixture.path(), "file.txt", "line one\n");
    let second = commit_file(fixture.path(), "file.txt", "line one\nline two\n");
    let root = LocalRoot::open(fixture.path()).expect("opens");
    members::add(&root, "reviewer", None, Some(fixture.key_path.clone())).expect("enrolls");

    let initial = NewReview {
        target: first.to_string(),
        verdict: Verdict::RequestChanges,
        body: "please address this".to_owned(),
    };
    let first_target =
        review::new(&root, initial, Some(fixture.key_path.clone())).expect("reviews");
    assert_eq!(first_target, first.to_string());

    // Re-review the descendant: the CLI's own signer/member resolution
    // finds the existing "reviewer" review of `first`, an ancestor of
    // `second`, and advances it in place.
    let follow_up = NewReview {
        target: second.to_string(),
        verdict: Verdict::Approve,
        body: "looks good now".to_owned(),
    };
    let advanced_target =
        review::new(&root, follow_up, Some(fixture.key_path.clone())).expect("re-reviews");

    // The composite key's target segment is unchanged (still genesis-keyed
    // at `first`), but the entity's own recorded target has moved to
    // `second`, and there is still exactly one review by this reviewer.
    assert_eq!(advanced_target, first_target);
    let (review, _thread) = review::show(&root, &first_target, "reviewer").expect("shows");
    assert_eq!(review.target(), second);
    assert_eq!(review.verdict, Verdict::Approve);
    assert_eq!(review.body, "looks good now");

    let all = review::list(&root, None).expect("lists");
    assert_eq!(
        all.len(),
        1,
        "re-review advances in place, not a second row"
    );
}
