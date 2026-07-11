//! Forge transfer: fetch and push over `refs/meta/*` (`sync.forge-transfer`,
//! and the push side of `sync.pre-flight` / `sync.inbox-routing`).
//!
//! Strategy: **integration harness** — a "remote" and a "local" are each a
//! ref-store / object-store pair, and the properties are end-to-end: after a
//! fetch the destination can verify a signed tip entirely on its own (so
//! history and signatures came across), a divergence is reported rather than
//! silently overwritten, and a push gates the *remote* write on the same
//! gate while routing an unauthorized canonical push to the inbox.

#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use ents_gate::{Config, Update, Verdict, verify};
use ents_model::{Issue, MemberId, Provenance, namespace};
use ents_sync::transfer::{Pushed, fetch, push};
use ents_testutil::{
    CommitSpec, Keypair, MemRefStore, ObjectStore, enroll_member, write_commit, write_meta_entity,
};
use gix::refs::FullName;
use gix_ref_store::RefStoreRead;

fn issue(state: &str) -> Issue {
    Issue {
        title: "t".into(),
        body: "b".into(),
        state: state.into(),
        assignees: vec![],
        labels: vec![],
    }
}

/// Enroll `admin` (and optionally `bob`) and record the epoch.
fn boot(refs: &MemRefStore, objects: &ObjectStore, admin: &Keypair, bob: Option<&Keypair>) {
    enroll_member(
        refs,
        objects,
        "admin",
        admin,
        Provenance::AdminRegistered,
        100,
    );
    let config: FullName = namespace::CONFIG_REF.try_into().unwrap();
    write_meta_entity(
        refs,
        objects,
        config,
        &Config { epoch: Some(200) },
        Some(admin),
        200,
    );
    if let Some(bob) = bob {
        enroll_member(refs, objects, "bob", bob, Provenance::SelfAttested, 250);
    }
}

/// Fetch moves the whole forge: every meta-ref, its full history, and the
/// signatures needed to verify it, so the destination verifies a tip on its
/// own with nothing left behind (`sync.forge-transfer`).
// @relation(sync.forge-transfer, scope=function, role=Verifies)
#[test]
fn fetch_moves_the_whole_forge_with_verifiable_signatures() {
    let remote_refs = MemRefStore::default();
    let remote_objects = ObjectStore::default();
    let admin = Keypair::from_seed(1);
    boot(&remote_refs, &remote_objects, &admin, None);

    // An issue with two commits of history.
    let name: FullName = "refs/meta/issues/1".try_into().unwrap();
    let parent = write_meta_entity(
        &remote_refs,
        &remote_objects,
        name.clone(),
        &issue("open"),
        Some(&admin),
        300,
    );
    let tip = write_meta_entity(
        &remote_refs,
        &remote_objects,
        name.clone(),
        &issue("closed"),
        Some(&admin),
        400,
    );

    let local_refs = MemRefStore::default();
    let local_objects = ObjectStore::default();
    let report = fetch(&remote_refs, &remote_objects, &local_refs, &local_objects).unwrap();

    // member, config, and the issue all arrived.
    assert!(
        report
            .updated
            .iter()
            .any(|n| n.as_bstr() == "refs/meta/member/admin")
    );
    assert!(
        report
            .updated
            .iter()
            .any(|n| n.as_bstr() == "refs/meta/config")
    );
    assert_eq!(local_refs.get(name.as_ref()).unwrap(), Some(tip));

    // The full history came with it, not just the tip.
    assert!(
        local_objects.get(&parent).is_some(),
        "the parent commit must transfer too"
    );

    // The signatures and policy transferred: the destination verifies the
    // tip against its *own* fetched state, offline.
    let snapshot = local_refs.fetched_copy();
    snapshot.remove(name.as_ref());
    let verdict = verify(
        &snapshot,
        &local_objects,
        &Update {
            name,
            new: Some(tip),
        },
    )
    .unwrap();
    assert!(matches!(verdict, Verdict::Pass(_)), "{verdict:?}");
}

/// When a local meta-ref has moved out from under the remote — neither tip
/// descends from the other — fetch reports the divergence for the merge
/// machinery to resolve, and does not clobber the local ref.
// @relation(sync.forge-transfer, scope=function, role=Verifies)
#[test]
fn fetch_reports_divergence_instead_of_overwriting() {
    let name: FullName = "refs/meta/issues/1".try_into().unwrap();

    let remote_refs = MemRefStore::default();
    let remote_objects = ObjectStore::default();
    let key = Keypair::from_seed(1);
    let remote_tip = write_meta_entity(
        &remote_refs,
        &remote_objects,
        name.clone(),
        &issue("open"),
        Some(&key),
        300,
    );

    // The local ref points at an independent-root commit: no descent either
    // way.
    let local_refs = MemRefStore::default();
    let local_objects = ObjectStore::default();
    let local_tip = {
        let tree = facet_git_tree::serialize_into(&issue("closed"), &local_objects).unwrap();
        write_commit(
            &local_objects,
            &CommitSpec {
                tree,
                parents: vec![],
                message: "local".into(),
                seconds: 300,
            },
            Some(&key),
        )
    };
    local_refs.set(name.as_ref(), local_tip);

    let report = fetch(&remote_refs, &remote_objects, &local_refs, &local_objects).unwrap();
    assert!(report.updated.is_empty());
    let diverged = report
        .diverged
        .iter()
        .find(|d| d.name == name)
        .expect("divergence reported");
    assert_eq!(diverged.local, local_tip);
    assert_eq!(diverged.remote, remote_tip);
    // The local ref is untouched.
    assert_eq!(local_refs.get(name.as_ref()).unwrap(), Some(local_tip));
}

/// Push pre-flights against the remote's own policy: an authorized push is
/// transferred and advances the remote ref (`sync.pre-flight`,
/// `sync.forge-transfer`).
// @relation(sync.pre-flight, sync.forge-transfer, scope=function, role=Verifies)
#[test]
fn push_advances_the_remote_on_an_authorized_ref() {
    let admin = Keypair::from_seed(1);
    let remote_refs = MemRefStore::default();
    let remote_objects = ObjectStore::default();
    boot(&remote_refs, &remote_objects, &admin, None);

    let local_refs = MemRefStore::default();
    let local_objects = ObjectStore::default();
    boot(&local_refs, &local_objects, &admin, None);

    let name: FullName = "refs/meta/issues/1".try_into().unwrap();
    let tip = write_meta_entity(
        &local_refs,
        &local_objects,
        name.clone(),
        &issue("open"),
        Some(&admin),
        300,
    );

    let pushed = push(
        &remote_refs,
        &remote_objects,
        &local_objects,
        &name,
        tip,
        &MemberId::new("admin"),
    )
    .unwrap();
    assert_eq!(pushed, Pushed::Advanced(name.clone()));
    assert_eq!(remote_refs.get(name.as_ref()).unwrap(), Some(tip));
}

/// A self-attested contributor's canonical push is predicted to fail, so
/// push routes it to the contributor's own inbox segment and leaves the
/// remote canonical ref untouched (`sync.inbox-routing`).
// @relation(sync.inbox-routing, sync.pre-flight, scope=function, role=Verifies)
#[test]
fn push_routes_an_unauthorized_canonical_push_to_the_inbox() {
    let admin = Keypair::from_seed(1);
    let bob = Keypair::from_seed(2);
    let remote_refs = MemRefStore::default();
    let remote_objects = ObjectStore::default();
    boot(&remote_refs, &remote_objects, &admin, Some(&bob));

    let local_refs = MemRefStore::default();
    let local_objects = ObjectStore::default();
    boot(&local_refs, &local_objects, &admin, Some(&bob));

    let name: FullName = "refs/meta/issues/1".try_into().unwrap();
    let tip = write_meta_entity(
        &local_refs,
        &local_objects,
        name.clone(),
        &issue("open"),
        Some(&bob),
        300,
    );

    let pushed = push(
        &remote_refs,
        &remote_objects,
        &local_objects,
        &name,
        tip,
        &MemberId::new("bob"),
    )
    .unwrap();
    match pushed {
        Pushed::Inbox(route) => assert_eq!(route.as_bstr(), "refs/meta/inbox/bob/issues/1"),
        other => panic!("expected inbox routing, got {other:?}"),
    }
    assert_eq!(
        remote_refs.get(name.as_ref()).unwrap(),
        None,
        "canonical ref must be untouched"
    );
}
