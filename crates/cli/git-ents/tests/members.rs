//! Integration coverage for `git ents members` against a real local
//! composition root (`roots.local`) — the bootstrap enrollment, then the
//! full add/revoke/unrevoke/check lifecycle atop it.
//!
//! rstest table-driven: the spec enumerates member-state transitions
//! (`model.member-revocation`) as a small closed set of cases, exactly the
//! shape the engineering conventions call out for table tests rather than
//! property tests.
#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "integration test"
)]

mod common;

use ents_model::MemberState;
use git_ents::commands::members;
use git_ents::root::LocalRoot;
use rstest::rstest;

/// The bootstrap window (`gate.bootstrap`) admits the very first member
/// with no prior enrollment — [`git_ents::lib`]'s own doctest exercises
/// this too; this test additionally confirms `git ents members list` then
/// reads it back through the real composition root.
// @relation(roots.local, model.member-identity, scope=function, role=Verifies)
#[test]
fn bootstrap_enrolls_the_first_member() {
    let fixture = common::Fixture::new(1);
    let root = LocalRoot::open(fixture.path()).expect("opens");

    members::add(&root, "jdc", None, Some(fixture.key_path.clone())).expect("bootstrap admits it");

    let listed = members::list(&root.refs, &root.objects).expect("lists");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].0, "jdc");
    assert_eq!(listed[0].1.state, MemberState::Active);
}

/// Enroll an admin, then use that same key to add a second member — the
/// ordinary (non-bootstrap) admin-registered path.
// @relation(roots.local, model.member-identity, scope=function, role=Verifies)
#[test]
fn admin_enrolls_a_second_member() {
    let fixture = common::Fixture::new(2);
    let root = LocalRoot::open(fixture.path()).expect("opens");
    members::add(&root, "admin", None, Some(fixture.key_path.clone())).expect("bootstrap");

    members::add(
        &root,
        "bob",
        Some("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBogus bob".to_owned()),
        Some(fixture.key_path.clone()),
    )
    .expect("admin-registered add");

    let listed = members::list(&root.refs, &root.objects).expect("lists");
    assert_eq!(listed.len(), 2);
    assert!(listed.iter().any(|(name, _)| name == "bob"));
}

#[rstest]
#[case::revoke_then_check(true)]
#[case::unrevoke_then_check(false)]
// @relation(model.member-revocation, scope=function, role=Verifies)
fn revoke_and_unrevoke_round_trip(#[case] end_revoked: bool) {
    let fixture = common::Fixture::new(3);
    let root = LocalRoot::open(fixture.path()).expect("opens");
    members::add(&root, "jdc", None, Some(fixture.key_path.clone())).expect("bootstrap");

    members::set_revoked(&root, "jdc", true, Some(fixture.key_path.clone())).expect("revoke");
    if !end_revoked {
        members::set_revoked(&root, "jdc", false, Some(fixture.key_path.clone()))
            .expect("unrevoke");
    }

    let (_, state) = members::check(&root, Some(fixture.key_path.clone()))
        .expect("reads")
        .expect("still a member record (revocation records state, never deletes)");
    let expected = if end_revoked {
        MemberState::Revoked
    } else {
        MemberState::Active
    };
    assert_eq!(state, expected);
}

/// `git ents members remove` deletes the ref entirely, distinct from
/// revocation (`model.member-revocation`'s "never deletes" contrast).
// @relation(model.member-revocation, scope=function, role=Verifies)
#[test]
fn remove_deletes_the_member_ref_entirely() {
    let fixture = common::Fixture::new(4);
    let root = LocalRoot::open(fixture.path()).expect("opens");
    members::add(&root, "jdc", None, Some(fixture.key_path.clone())).expect("bootstrap");

    members::remove(&root, "jdc", Some(fixture.key_path.clone())).expect("removes");

    let listed = members::list(&root.refs, &root.objects).expect("lists");
    assert!(listed.is_empty());
}
