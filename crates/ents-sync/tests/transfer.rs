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

/// A ref store that simulates a racing writer: the first transaction it
/// receives is preceded by a competing ref move, landing exactly in the
/// window between a caller's read (or pre-flight) and its CAS.
struct RacingStore<'a> {
    inner: &'a MemRefStore,
    race: std::sync::Mutex<Option<(FullName, gix_hash::ObjectId)>>,
}

impl<'a> RacingStore<'a> {
    fn new(inner: &'a MemRefStore, name: FullName, oid: gix_hash::ObjectId) -> Self {
        Self {
            inner,
            race: std::sync::Mutex::new(Some((name, oid))),
        }
    }
}

impl gix_ref_store::RefStoreRead for RacingStore<'_> {
    fn get(
        &self,
        name: &gix::refs::FullNameRef,
    ) -> gix_ref_store::Result<Option<gix_hash::ObjectId>> {
        self.inner.get(name)
    }

    fn iter_prefix(&self, prefix: &str) -> gix_ref_store::Result<gix_ref_store::RefIter> {
        self.inner.iter_prefix(prefix)
    }
}

impl gix_ref_store::RefStore for RacingStore<'_> {
    #[expect(
        clippy::unwrap_in_result,
        reason = "test fixture: a poisoned mutex is a broken test, not a condition under test"
    )]
    fn transaction(
        &self,
        edits: &[gix_ref_store::RefEdit],
    ) -> gix_ref_store::Result<gix_ref_store::TxOutcome> {
        if let Some((name, oid)) = self.race.lock().unwrap().take() {
            self.inner.set(name.as_ref(), oid);
        }
        self.inner.transaction(edits)
    }
}

/// The staleness race pre-flight admits (`sync.pre-flight`: "a prediction
/// that can only be stale"): another writer advances the remote ref between
/// the passing verdict and the CAS. The rejected transaction must surface
/// as [`Pushed::Stale`] — never as a fabricated success — and the racing
/// writer's tip must survive untouched.
// @relation(sync.pre-flight, scope=function, role=Verifies)
#[test]
fn push_reports_a_lost_cas_race_as_stale_not_success() {
    let admin = Keypair::from_seed(1);
    let remote_refs = MemRefStore::default();
    let remote_objects = ObjectStore::default();
    boot(&remote_refs, &remote_objects, &admin, None);

    let local_refs = MemRefStore::default();
    let local_objects = ObjectStore::default();
    boot(&local_refs, &local_objects, &admin, None);

    let name: FullName = "refs/meta/issues/1".try_into().unwrap();
    let ours = write_meta_entity(
        &local_refs,
        &local_objects,
        name.clone(),
        &issue("open"),
        Some(&admin),
        300,
    );

    // The racing writer's competing tip, landed on the remote the instant
    // push's transaction begins — after pre-flight has already passed.
    let racer = {
        let tree = facet_git_tree::serialize_into(&issue("closed"), &remote_objects).unwrap();
        write_commit(
            &remote_objects,
            &CommitSpec {
                tree,
                parents: vec![],
                message: "racer".into(),
                seconds: 310,
            },
            Some(&admin),
        )
    };
    let racing = RacingStore::new(&remote_refs, name.clone(), racer);

    let pushed = push(
        &racing,
        &remote_objects,
        &local_objects,
        &name,
        ours,
        &MemberId::new("admin"),
    )
    .unwrap();

    assert_eq!(
        pushed,
        Pushed::Stale(name.clone()),
        "a lost CAS race must not be reported as Advanced"
    );
    assert_eq!(
        remote_refs.get(name.as_ref()).unwrap(),
        Some(racer),
        "the racing writer's tip must survive; nothing was written"
    );
}

/// The same race on the fetch side: a local writer moves the ref between
/// fetch's read and its transaction. The ref must land in
/// [`ents_sync::transfer::FetchReport::stale`], never in `updated`, and the
/// concurrent writer's tip must survive.
// @relation(sync.forge-transfer, scope=function, role=Verifies)
#[test]
fn fetch_reports_a_lost_cas_race_as_stale_not_updated() {
    let key = Keypair::from_seed(1);
    let remote_refs = MemRefStore::default();
    let remote_objects = ObjectStore::default();
    let name: FullName = "refs/meta/issues/1".try_into().unwrap();
    write_meta_entity(
        &remote_refs,
        &remote_objects,
        name.clone(),
        &issue("open"),
        Some(&key),
        300,
    );

    let local_refs = MemRefStore::default();
    let local_objects = ObjectStore::default();
    // A concurrent local writer creates the same ref mid-fetch, defeating
    // the MustNotExist precondition fetch read moments earlier.
    let racer = {
        let tree = facet_git_tree::serialize_into(&issue("closed"), &local_objects).unwrap();
        write_commit(
            &local_objects,
            &CommitSpec {
                tree,
                parents: vec![],
                message: "racer".into(),
                seconds: 310,
            },
            Some(&key),
        )
    };
    let racing = RacingStore::new(&local_refs, name.clone(), racer);

    let report = fetch(&remote_refs, &remote_objects, &racing, &local_objects).unwrap();

    assert!(
        report.updated.is_empty(),
        "a rejected CAS must not be reported as updated: {report:?}"
    );
    assert_eq!(report.stale, vec![name.clone()]);
    assert_eq!(
        local_refs.get(name.as_ref()).unwrap(),
        Some(racer),
        "the concurrent writer's tip must survive; nothing was written"
    );
}
