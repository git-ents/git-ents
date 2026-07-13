//! Pre-flight, inbox routing, and the local-advisory boundary
//! (`sync.pre-flight`, `sync.inbox-routing`, `sync.local-advisory`).
//!
//! Strategy: **rstest table-driven** for the enumerable cases (pass vs the
//! two kinds of refusal, and the refname-mapping table for [`inbox_route`]),
//! plus targeted integration tests for the two invariants that are about
//! *identity* rather than a case: a pre-flight verdict equals the gate's own
//! verdict on the same inputs (`sync.pre-flight`), and a failing pre-flight
//! never blocks a local write (`sync.local-advisory`).

#![expect(clippy::unwrap_used, reason = "tests")]

use ents_gate::{Config, Update, verify};
use ents_model::{MemberId, Provenance, namespace};
use ents_sync::{inbox_route, preflight};
use ents_testutil::{
    CommitSpec, Keypair, MemRefStore, ObjectStore, enroll_member, write_commit, write_meta_entity,
};
use gix::refs::FullName;
use gix_hash::ObjectId;
use gix_ref_store::{Expected, RefEdit, RefStore, RefStoreRead};
use rstest::rstest;

/// A stand-in for `ents-forge`'s `Issue` (this crate cannot depend on
/// `ents-forge`): any multi-field entity exercises the same pre-flight and
/// inbox-routing machinery, which is generic over the typed tree.
#[derive(Debug, Clone, PartialEq, Eq, facet::Facet)]
struct Issue {
    title: String,
    body: String,
    state: String,
}

fn issue() -> Issue {
    Issue {
        title: "t".into(),
        body: "b".into(),
        state: "open".into(),
    }
}

/// Build a parentless, signed genesis issue commit and the oid-keyed
/// refname it binds (`meta-ref.identity-binding`). Does not touch the ref
/// store — pre-flight judges a proposal against a snapshot.
fn genesis_issue(objects: &ObjectStore, signer: &Keypair, seconds: i64) -> (FullName, ObjectId) {
    let tree = facet_git_tree::serialize_into(&issue(), objects).unwrap();
    let tip = write_commit(
        objects,
        &CommitSpec {
            tree,
            parents: vec![],
            message: "Open issue".into(),
            seconds,
        },
        Some(signer),
    );
    (format!("refs/meta/issues/{tip}").try_into().unwrap(), tip)
}

/// A child issue commit of `parent`, signed, for divergence scenarios.
fn child_issue(
    objects: &ObjectStore,
    parent: ObjectId,
    state: &str,
    signer: &Keypair,
    seconds: i64,
) -> ObjectId {
    let mut edited = issue();
    edited.state = state.into();
    let tree = facet_git_tree::serialize_into(&edited, objects).unwrap();
    write_commit(
        objects,
        &CommitSpec {
            tree,
            parents: vec![parent],
            message: "Edit issue".into(),
            seconds,
        },
        Some(signer),
    )
}

/// A booted forge (admin enrolled, epoch set) plus a self-attested bob.
fn forge() -> (MemRefStore, ObjectStore, Keypair, Keypair) {
    let refs = MemRefStore::default();
    let objects = ObjectStore::default();
    let admin = Keypair::from_seed(1);
    let bob = Keypair::from_seed(2);
    enroll_member(
        &refs,
        &objects,
        "admin",
        &admin,
        Provenance::AdminRegistered,
        100,
    );
    let config: FullName = namespace::CONFIG_REF.try_into().unwrap();
    write_meta_entity(
        &refs,
        &objects,
        config,
        &Config { epoch: Some(200) },
        Some(&admin),
        200,
    );
    enroll_member(&refs, &objects, "bob", &bob, Provenance::SelfAttested, 250);
    (refs, objects, admin, bob)
}

/// A pre-flight against a canonical push that the pusher is not authorized
/// for offers the author's own inbox route the instant the verdict goes
/// negative (`sync.inbox-routing`), while an authorized push offers none.
#[rstest]
#[case::authorized_pass(1, true, false)]
#[case::unauthorized_offers_inbox(2, false, true)]
// @relation(sync.pre-flight, sync.inbox-routing, scope=function, role=Verifies)
fn preflight_offers_inbox_only_on_an_authorization_refusal(
    #[case] seed: u8,
    #[case] expect_pass: bool,
    #[case] expect_inbox: bool,
) {
    let (refs, objects, admin, bob) = forge();
    let signer = if seed == 1 { &admin } else { &bob };
    let author = if seed == 1 { "admin" } else { "bob" };

    // The issue's id is its genesis commit's own oid; the ref does not yet
    // exist, so pre-flight judges a fresh creation.
    let (name, tip) = genesis_issue(&objects, signer, 300);
    let pf = preflight(
        &refs,
        &objects,
        &Update {
            name: name.clone(),
            new: Some(tip),
        },
        &MemberId::new(author),
    )
    .unwrap();

    assert_eq!(pf.is_pass(), expect_pass, "{:?}", pf.verdict);
    assert_eq!(pf.inbox.is_some(), expect_inbox);
    if let Some(inbox) = pf.inbox {
        let expected = format!("refs/meta/inbox/bob/issues/{tip}");
        assert_eq!(inbox.as_bstr(), expected.as_str());
    }
}

/// A fast-forward refusal is a *divergence* — the answer is a merge, not the
/// inbox — so pre-flight offers no inbox route for it, matching the gate's
/// own `inbox_alternative` signal (`sync.inbox-routing`).
// @relation(sync.inbox-routing, scope=function, role=Verifies)
#[test]
fn a_divergence_refusal_offers_no_inbox() {
    let (refs, objects, admin, _bob) = forge();
    // A genesis and two divergent children of it, all correctly bound to
    // the same oid-keyed ref (both descend from the same genesis, so
    // identity binding holds); the current tip is one child, and the
    // proposal is the sibling, which cannot descend from it — a genuine
    // fast-forward refusal, not an identity mismatch.
    let (name, genesis) = genesis_issue(&objects, &admin, 300);
    let current = child_issue(&objects, genesis, "open", &admin, 350);
    let sibling = child_issue(&objects, genesis, "closed", &admin, 350);
    refs.set(name.as_ref(), current);

    let pf = preflight(
        &refs,
        &objects,
        &Update {
            name: name.clone(),
            new: Some(sibling),
        },
        &MemberId::new("admin"),
    )
    .unwrap();
    assert!(!pf.is_pass(), "{:?}", pf.verdict);
    assert!(
        pf.inbox.is_none(),
        "a divergence routes to a merge, not the inbox"
    );
}

/// Pre-flight runs the *identical* gate function every call site runs, so
/// its verdict is always exactly the gate's verdict on the same inputs — a
/// prediction that can only be stale, never wrong about the rules
/// (`sync.pre-flight`, `gate.call-sites`).
// @relation(sync.pre-flight, scope=function, role=Verifies)
#[test]
fn preflight_verdict_equals_the_gate_verdict() {
    let (refs, objects, admin, bob) = forge();

    for (author, signer) in [("admin", &admin), ("bob", &bob)] {
        let (name, tip) = genesis_issue(&objects, signer, 300);
        let update = Update {
            name,
            new: Some(tip),
        };

        let pf = preflight(&refs, &objects, &update, &MemberId::new(author)).unwrap();
        let gate = verify(&refs, &objects, &update).unwrap();
        assert_eq!(
            pf.verdict, gate,
            "pre-flight must be the gate, not a copy of it"
        );
    }
}

/// Sync honors the gate's advisory role locally: a failing pre-flight is a
/// prediction, not a veto — the same commit still writes to the local store
/// (`sync.local-advisory`). The consequence sync owns is the inbox offer,
/// which the failing pre-flight already surfaced.
// @relation(sync.local-advisory, scope=function, role=Verifies)
#[test]
fn a_failing_preflight_never_blocks_the_local_write() {
    let (refs, objects, _admin, bob) = forge();
    let (name, tip) = genesis_issue(&objects, &bob, 300);

    let pf = preflight(
        &refs,
        &objects,
        &Update {
            name: name.clone(),
            new: Some(tip),
        },
        &MemberId::new("bob"),
    )
    .unwrap();
    assert!(!pf.is_pass());
    assert!(
        pf.inbox.is_some(),
        "the rejection consequence is an inbox offer"
    );

    // Nothing sync did prevents the local write from applying.
    let outcome = refs
        .transaction(&[RefEdit {
            name: name.clone(),
            expected: Expected::MustNotExist,
            new: Some(tip),
        }])
        .unwrap();
    assert_eq!(refs.get(name.as_ref()).unwrap(), Some(tip));
    let _ = outcome;
}

#[rstest]
#[case::canonical("refs/meta/issues/42", "refs/meta/inbox/jdc/issues/42")]
#[case::nested("refs/meta/results/unit/abc", "refs/meta/inbox/jdc/results/unit/abc")]
#[case::already_inbox("refs/meta/inbox/jdc/issues/1", "refs/meta/inbox/jdc/issues/1")]
#[case::non_meta("refs/heads/main", "refs/heads/main")]
// @relation(sync.inbox-routing, scope=function, role=Verifies)
fn inbox_route_maps_canonical_to_the_authors_segment(#[case] input: &str, #[case] expected: &str) {
    let canonical: FullName = input.try_into().unwrap();
    let routed = inbox_route(canonical.as_ref(), &MemberId::new("jdc")).unwrap();
    assert_eq!(routed.as_bstr(), expected);
}
