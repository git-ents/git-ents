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
use ents_model::trailer::Trailers;
use ents_model::{Issue, MemberId, Provenance, namespace};
use ents_sync::{inbox_route, preflight};
use ents_testutil::{
    CommitSpec, Keypair, MemRefStore, ObjectStore, enroll_member, write_commit, write_meta_entity,
};
use gix::refs::FullName;
use gix_ref_store::{Expected, RefEdit, RefStore, RefStoreRead};
use rstest::rstest;

fn issue() -> Issue {
    Issue {
        title: "t".into(),
        body: "b".into(),
        state: "open".into(),
        assignees: vec![],
        labels: vec![],
    }
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

    let name: FullName = "refs/meta/issues/9".try_into().unwrap();
    let tip = write_meta_entity(&refs, &objects, name.clone(), &issue(), Some(signer), 300);

    let before = refs.fetched_copy();
    before.remove(name.as_ref());
    let pf = preflight(
        &before,
        &objects,
        &Update {
            name,
            new: Some(tip),
        },
        &MemberId::new(author),
    )
    .unwrap();

    assert_eq!(pf.is_pass(), expect_pass);
    assert_eq!(pf.inbox.is_some(), expect_inbox);
    if let Some(inbox) = pf.inbox {
        assert_eq!(inbox.as_bstr(), "refs/meta/inbox/bob/issues/9");
    }
}

/// A fast-forward refusal is a *divergence* — the answer is a merge, not the
/// inbox — so pre-flight offers no inbox route for it, matching the gate's
/// own `inbox_alternative` signal (`sync.inbox-routing`).
// @relation(sync.inbox-routing, scope=function, role=Verifies)
#[test]
fn a_divergence_refusal_offers_no_inbox() {
    let (refs, objects, admin, _bob) = forge();
    let name: FullName = "refs/meta/issues/3".try_into().unwrap();
    write_meta_entity(&refs, &objects, name.clone(), &issue(), Some(&admin), 300);

    // An independent root correctly bound to the same ref: signed by an
    // authorized member with a matching trailer, but with no parents, so it
    // cannot descend from the current tip — a genuine fast-forward refusal.
    let mut other = issue();
    other.state = "closed".into();
    let sibling = {
        let tree = facet_git_tree::serialize_into(&other, &objects).unwrap();
        let trailers = Trailers {
            ents_ref: Some(name.clone()),
            schema_version: None,
        };
        let message = format!("Mutate {}\n\n{}", name.as_bstr(), trailers.render());
        write_commit(
            &objects,
            &CommitSpec {
                tree,
                parents: vec![],
                message,
                seconds: 350,
            },
            Some(&admin),
        )
    };

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
    assert!(!pf.is_pass());
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
        let name: FullName = format!("refs/meta/issues/{author}").try_into().unwrap();
        let tip = write_meta_entity(&refs, &objects, name.clone(), &issue(), Some(signer), 300);
        let before = refs.fetched_copy();
        before.remove(name.as_ref());
        let update = Update {
            name,
            new: Some(tip),
        };

        let pf = preflight(&before, &objects, &update, &MemberId::new(author)).unwrap();
        let gate = verify(&before, &objects, &update).unwrap();
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
    let name: FullName = "refs/meta/issues/9".try_into().unwrap();
    let tip = write_meta_entity(&refs, &objects, name.clone(), &issue(), Some(&bob), 300);

    let before = refs.fetched_copy();
    before.remove(name.as_ref());
    let pf = preflight(
        &before,
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
    let outcome = before
        .transaction(&[RefEdit {
            name: name.clone(),
            expected: Expected::MustNotExist,
            new: Some(tip),
        }])
        .unwrap();
    assert_eq!(before.get(name.as_ref()).unwrap(), Some(tip));
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
