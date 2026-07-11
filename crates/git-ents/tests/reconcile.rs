//! The phase-6 exit criterion, run literally: "the boot-time
//! reconciliation scan regenerates obligations correctly after a `kill -9`
//! of the in-memory queue."
//!
//! [`HostedRoot::open`] runs [`ents_receive::reconcile`] at open time
//! (`receive.reconstructible`) to populate its in-memory `EventSink`; this
//! test defines an effect, advances a branch into its trigger set, opens a
//! `HostedRoot` once, then *drops it and opens a fresh one* against the
//! same on-disk repository — standing in for a `kill -9` of whatever
//! process held the in-memory queue, since nothing here persists across
//! that boundary except repository state itself.
#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::string_slice,
    reason = "integration test"
)]

mod common;

use ents_model::Effect;
use git_ents::root::HostedRoot;
use gix_object::{Commit, Kind, Write as _};
use gix_ref_store::{Expected, RefEdit, RefStore};

/// Write an empty-tree commit and move `refname` to it directly through
/// the ref store — a branch ref needs no signature at all
/// (`gate.principled-split`: code refs keep transport-level authorization,
/// never the tip invariant), so this bypasses `receive` entirely, the way
/// a plain `git commit` would.
fn advance_branch(root: &HostedRoot, refname: &str, seconds: i64) -> gix_hash::ObjectId {
    let empty_tree = root
        .objects
        .write(&gix_object::Tree::empty())
        .expect("tree");
    let actor = gix::actor::Signature {
        name: "test".into(),
        email: "test@ents.test".into(),
        time: gix::date::Time { seconds, offset: 0 },
    };
    let commit = Commit {
        tree: empty_tree,
        parents: Default::default(),
        author: actor.clone(),
        committer: actor,
        encoding: None,
        message: "advance".into(),
        extra_headers: Vec::new(),
    };
    let mut raw = Vec::new();
    gix_object::WriteTo::write_to(&commit, &mut raw).expect("serialize");
    let oid = root.objects.write_buf(Kind::Commit, &raw).expect("write");

    let name: gix::refs::FullName = refname.try_into().expect("valid refname");
    root.refs
        .transaction(&[RefEdit {
            name,
            expected: Expected::Any,
            new: Some(oid),
        }])
        .expect("moves the ref");
    oid
}

fn define_effect(root: &HostedRoot, name: &str, trigger: &str) {
    let tree = facet_git_tree::serialize_into(
        &Effect {
            trigger: trigger.to_owned(),
            toolchains: vec![],
            run: "true".to_owned(),
        },
        &root.objects,
    )
    .expect("serialize effect");
    let commit = Commit {
        tree,
        parents: Default::default(),
        author: gix::actor::Signature {
            name: "test".into(),
            email: "test@ents.test".into(),
            time: gix::date::Time {
                seconds: 1,
                offset: 0,
            },
        },
        committer: gix::actor::Signature {
            name: "test".into(),
            email: "test@ents.test".into(),
            time: gix::date::Time {
                seconds: 1,
                offset: 0,
            },
        },
        encoding: None,
        message: "define effect".into(),
        extra_headers: Vec::new(),
    };
    let mut raw = Vec::new();
    gix_object::WriteTo::write_to(&commit, &mut raw).expect("serialize");
    let oid = root.objects.write_buf(Kind::Commit, &raw).expect("write");

    let ref_name = ents_model::namespace::effect_ref(name).expect("valid");
    root.refs
        .transaction(&[RefEdit {
            name: ref_name,
            expected: Expected::Any,
            new: Some(oid),
        }])
        .expect("moves the ref");
}

/// The literal phase-6 exit test.
// @relation(receive.reconstructible, scope=function, role=Verifies)
#[test]
fn boot_time_reconciliation_survives_a_simulated_crash() {
    let fixture = common::Fixture::new_bare(10);

    // Set up repository state entirely before any `HostedRoot` exists —
    // "queue" state here is purely derived, never authored directly.
    {
        let root = HostedRoot::open(fixture.path()).expect("opens");
        define_effect(&root, "unit", "rev(refs/heads/main)");
        advance_branch(&root, "refs/heads/main", 100);
        // This first root's own boot scan already saw the commit (it
        // opened after the effect existed but the commit came after)...
    }
    // ...so open fresh once more with everything in place, exactly as a
    // process starting for the first time against this repository would.
    let expected_oid = {
        let root = HostedRoot::open(fixture.path()).expect("second open reconciles fresh");
        let pending = root.events.pending();
        assert_eq!(pending.len(), 1, "exactly one outstanding obligation");
        assert_eq!(pending[0].0, "unit");
        pending[0].1
    };

    // Simulate `kill -9` of the in-memory queue: drop this handle (its
    // `MemoryEventSink` goes with it — nothing persists it) and open an
    // entirely fresh `HostedRoot` against the same on-disk repository.
    let root = HostedRoot::open(fixture.path()).expect("reconciles again, from scratch");
    let pending = root.events.pending();
    assert_eq!(
        pending,
        vec![("unit".to_owned(), expected_oid)],
        "the boot-time scan regenerates the exact same obligation from repository state alone"
    );
}

/// Once a result exists for a commit, reconciliation must not re-list it —
/// otherwise a restarted worker would re-run every effect it had ever
/// completed.
// @relation(receive.reconstructible, query.workset, scope=function, role=Verifies)
#[test]
fn reconciliation_excludes_already_resulted_commits() {
    let fixture = common::Fixture::new_bare(11);
    let root = HostedRoot::open(fixture.path()).expect("opens");
    define_effect(&root, "unit", "rev(refs/heads/main)");
    let oid = advance_branch(&root, "refs/heads/main", 100);

    // Record a result directly (bypassing `write_result`'s signing
    // requirement — this test only needs the ref to exist).
    let short = &oid.to_string()[..12];
    let status_tree = facet_git_tree::serialize_into(&ents_model::Status::Pass, &root.objects)
        .expect("serialize");
    let commit = gix_object::Commit {
        tree: status_tree,
        parents: Default::default(),
        author: gix::actor::Signature {
            name: "worker".into(),
            email: "worker@ents.test".into(),
            time: gix::date::Time {
                seconds: 200,
                offset: 0,
            },
        },
        committer: gix::actor::Signature {
            name: "worker".into(),
            email: "worker@ents.test".into(),
            time: gix::date::Time {
                seconds: 200,
                offset: 0,
            },
        },
        encoding: None,
        message: "result".into(),
        extra_headers: Vec::new(),
    };
    let mut raw = Vec::new();
    gix_object::WriteTo::write_to(&commit, &mut raw).expect("serialize");
    let result_oid = root.objects.write_buf(Kind::Commit, &raw).expect("write");
    let result_ref = ents_model::namespace::result_ref("unit", short).expect("valid");
    root.refs
        .transaction(&[RefEdit {
            name: result_ref,
            expected: Expected::Any,
            new: Some(result_oid),
        }])
        .expect("records the result");

    let root = HostedRoot::open(fixture.path()).expect("reconciles fresh");
    assert!(
        root.events.pending().is_empty(),
        "a commit with a recorded result must never be re-enqueued"
    );
}
