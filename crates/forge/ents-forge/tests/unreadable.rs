//! Coverage for [`ents_forge::comment::list_all`] and
//! [`ents_forge::issue::list_all`]: a listing returns the refs it could
//! not read back alongside its readable rows ([`ents_forge::Unreadable`])
//! rather than dropping them on the floor — the fixture writes one
//! good-shape entity and one wrong-shape ref (an empty tree no entity
//! decoder accepts) under the same prefix and asserts both surface.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "integration test: fixtures panic on setup failure"
)]

use ents_forge::Issue;
use ents_forge::comment::Comment;
use ents_model::MemberId;
use ents_testutil::{CommitSpec, MemRefStore, ObjectStore, empty_tree, write_commit};

/// A commit whose tree is empty — a shape no entity decoder reads back —
/// landed on `refname`, standing in for a ref written by an older or
/// unrelated schema.
fn seed_wrong_shape_ref(refs: &MemRefStore, objects: &ObjectStore, refname: &str) {
    let spec = CommitSpec {
        tree: empty_tree(objects),
        parents: vec![],
        message: "Wrong-shape entity".into(),
        seconds: 1_000,
    };
    let tip = write_commit(objects, &spec, None);
    refs.set_str(refname, tip);
}

#[test]
fn comment_list_all_returns_an_unreadable_ref_alongside_a_readable_row() {
    let refs = MemRefStore::default();
    let objects = ObjectStore::default();

    let good = Comment {
        body: "readable".to_owned(),
        state: "open".to_owned(),
        anchor: None,
        context: Some("issues/42".to_owned()),
        parent: None,
    };
    let name: gix::refs::FullName = "refs/meta/comments/good".try_into().expect("valid refname");
    ents_testutil::write_meta_entity(&refs, &objects, name, &good, None, 1_000);
    seed_wrong_shape_ref(&refs, &objects, "refs/meta/comments/legacy");

    let (rows, unreadable) = ents_forge::comment::list_all(&refs, &objects).expect("listing reads");
    assert_eq!(rows.len(), 1, "the readable comment still lists");
    assert_eq!(rows[0].0, "good");
    assert_eq!(unreadable.len(), 1, "the wrong-shape ref surfaces");
    assert_eq!(unreadable[0].refname, "refs/meta/comments/legacy");
    assert!(
        !unreadable[0].error.is_empty(),
        "the deserialization error text comes along for diagnosis"
    );

    // `list` stays the readable-rows-only view of the same walk: the
    // wrong-shape ref is absent, and nothing errors.
    let listed = ents_forge::comment::list(&refs, &objects).expect("listing reads");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].0, "good");
}

#[test]
fn issue_list_all_returns_an_unreadable_ref_alongside_a_readable_row() {
    let refs = MemRefStore::default();
    let objects = ObjectStore::default();

    let good = Issue {
        title: "readable".to_owned(),
        body: String::new(),
        state: "open".to_owned(),
        assignees: vec![MemberId::new("jdc")],
        labels: vec![],
    };
    let name: gix::refs::FullName = "refs/meta/issues/good".try_into().expect("valid refname");
    ents_testutil::write_meta_entity(&refs, &objects, name, &good, None, 1_000);
    seed_wrong_shape_ref(&refs, &objects, "refs/meta/issues/legacy");

    let (rows, unreadable) = ents_forge::issue::list_all(&refs, &objects).expect("listing reads");
    assert_eq!(rows.len(), 1, "the readable issue still lists");
    assert_eq!(rows[0].0, "good");
    assert_eq!(unreadable.len(), 1, "the wrong-shape ref surfaces");
    assert_eq!(unreadable[0].refname, "refs/meta/issues/legacy");
}
