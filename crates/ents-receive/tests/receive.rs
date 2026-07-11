//! Integration tests for `receive`: the mandatory/advisory gate-policy
//! table (rstest — the spec's two named policies), redaction enforcement
//! (admin-only push, ingest refusal), `(effect, oid)` dedup, and the
//! reconstructibility proof that a boot-time [`reconcile`] rebuilds exactly
//! the obligations incremental `receive` calls would have enqueued.

#![expect(
    clippy::expect_used,
    reason = "integration test: fixtures panic on setup failure"
)]

use ents_gate::Config;
use ents_model::{Effect, Issue, Provenance, Redaction, namespace, trailer::Trailers};
use ents_receive::{
    MemoryEventSink, Mode, NullEventSink, Proposal, RefTransition, TxResult, receive, reconcile,
};
use ents_testutil::{
    CommitSpec, Keypair, MemRefStore, ObjectStore, enroll_member, write_commit, write_meta_entity,
};
use gix::refs::FullName;
use gix_hash::ObjectId;
use gix_object::{Kind, Write as _};
use gix_ref_store::RefStoreRead as _;
use rstest::rstest;

const ADMIN_SEED: u8 = 1;
const GUEST_SEED: u8 = 2;

/// A forge fixture with verification in force: an admin-registered member
/// `admin`, a self-attested member `guest`, and an epoch recorded in
/// `refs/meta/config` — the same shape `ents-gate`'s own tests use.
struct Forge {
    refs: MemRefStore,
    objects: ObjectStore,
    admin: Keypair,
    guest: Keypair,
}

fn forge() -> Forge {
    let refs = MemRefStore::default();
    let objects = ObjectStore::default();
    let admin = Keypair::from_seed(ADMIN_SEED);
    let guest = Keypair::from_seed(GUEST_SEED);
    enroll_member(
        &refs,
        &objects,
        "admin",
        &admin,
        Provenance::AdminRegistered,
        100,
    );
    enroll_member(
        &refs,
        &objects,
        "guest",
        &guest,
        Provenance::SelfAttested,
        110,
    );
    let config_ref: FullName = namespace::CONFIG_REF.try_into().expect("valid");
    write_meta_entity(
        &refs,
        &objects,
        config_ref,
        &Config { epoch: Some(200) },
        Some(&admin),
        200,
    );
    Forge {
        refs,
        objects,
        admin,
        guest,
    }
}

fn name(s: &str) -> FullName {
    s.try_into().expect("valid refname in test")
}

/// Build a signed (or unsigned) mutation commit that binds itself to
/// `refname` via the `Advance-ref:` trailer, *without* moving the ref —
/// unlike `ents_testutil::write_meta_entity`, so the test can hand the
/// result to `receive` and observe whether *it* moves the ref.
fn build_mutation<T: for<'facet> facet::Facet<'facet>>(
    objects: &ObjectStore,
    refname: &FullName,
    entity: &T,
    signer: Option<&Keypair>,
    seconds: i64,
) -> ObjectId {
    let tree = facet_git_tree::serialize_into(entity, objects).expect("serializes");
    let trailers = Trailers {
        ents_ref: Some(refname.clone()),
        schema_version: None,
    };
    let message = format!("Mutate {}\n\n{}", refname.as_bstr(), trailers.render());
    write_commit(
        objects,
        &CommitSpec {
            tree,
            parents: vec![],
            message,
            seconds,
        },
        signer,
    )
}

fn single(transition: RefTransition, objects: Vec<ObjectId>) -> Proposal {
    Proposal {
        transitions: vec![transition],
        objects,
        auth: None,
    }
}

// ---------------------------------------------------------------------
// receive.unit, receive.shared-path, receive.refstore-seam: the gate
// policy table — mandatory aborts the whole batch on a failing verdict,
// advisory writes regardless and only annotates.
// ---------------------------------------------------------------------

#[rstest]
#[case::mandatory_authorized(Mode::Mandatory, true, TxResult::Applied, true)]
#[case::mandatory_unauthorized(Mode::Mandatory, false, TxResult::Refused, false)]
#[case::advisory_authorized(Mode::Advisory, true, TxResult::Applied, true)]
#[case::advisory_unauthorized(Mode::Advisory, false, TxResult::Applied, false)]
// @relation(receive.unit, receive.shared-path, receive.refstore-seam, receive.object-access, receive.proposal-shape, gate.mandatory-hosted, gate.advisory-local, scope=function, role=Verifies)
fn gate_policy_matches_mode(
    #[case] mode: Mode,
    #[case] authorized: bool,
    #[case] expected: TxResult,
    #[case] expect_verdict_pass: bool,
) {
    let forge = forge();
    let refname = namespace::issue_ref("1").expect("valid");
    let signer = if authorized {
        &forge.admin
    } else {
        &forge.guest
    };
    let issue = Issue {
        title: "t".into(),
        body: "b".into(),
        state: "open".into(),
        assignees: vec![],
        labels: vec![],
    };
    let tip = build_mutation(&forge.objects, &refname, &issue, Some(signer), 300);

    let outcome = receive(
        &forge.refs,
        &forge.objects,
        &NullEventSink,
        &single(
            RefTransition {
                name: refname.clone(),
                old: None,
                new: Some(tip),
            },
            vec![tip],
        ),
        mode,
    )
    .expect("evaluates");

    assert_eq!(outcome.result, expected);
    assert_eq!(outcome.verdicts.len(), 1);
    let (_, verdict) = outcome.verdicts.first().expect("exactly one transition");
    assert_eq!(verdict.is_pass(), expect_verdict_pass);

    let landed = forge
        .refs
        .get(refname.as_ref())
        .expect("readable")
        .is_some();
    assert_eq!(landed, matches!(expected, TxResult::Applied));
}

// ---------------------------------------------------------------------
// receive.redaction-admin-only: a push to refs/meta/redactions/* is
// refused unless the pusher is admin-registered — a consequence of
// composing receive with ents-gate's existing authorization arm, pinned
// here at the receive level.
// ---------------------------------------------------------------------

#[rstest]
#[case::admin_registered(true, TxResult::Applied)]
#[case::self_attested(false, TxResult::Refused)]
// @relation(receive.redaction-admin-only, scope=function, role=Verifies)
fn redaction_ref_push_requires_admin(#[case] as_admin: bool, #[case] expected: TxResult) {
    let forge = forge();
    let refname = namespace::redaction_ref("r1").expect("valid");
    let signer = if as_admin { &forge.admin } else { &forge.guest };
    let redaction = Redaction::new(ObjectId::null(gix_hash::Kind::Sha1), "leaked credential");
    let tip = build_mutation(&forge.objects, &refname, &redaction, Some(signer), 300);

    let outcome = receive(
        &forge.refs,
        &forge.objects,
        &NullEventSink,
        &single(
            RefTransition {
                name: refname,
                old: None,
                new: Some(tip),
            },
            vec![tip],
        ),
        Mode::Mandatory,
    )
    .expect("evaluates");

    assert_eq!(outcome.result, expected);
}

// ---------------------------------------------------------------------
// receive.redaction-ingest: a redacted hole cannot be silently refilled
// by re-pushing the same bytes.
// ---------------------------------------------------------------------

#[rstest]
// @relation(receive.redaction-ingest, scope=function, role=Verifies)
fn reintroducing_a_redacted_object_refuses_the_whole_batch() {
    let forge = forge();

    // A blob that was, at some point, the payload of a leaked credential.
    let leaked = forge
        .objects
        .write_buf(Kind::Blob, b"super secret")
        .expect("write");
    let redaction_ref = namespace::redaction_ref("r1").expect("valid");
    write_meta_entity(
        &forge.refs,
        &forge.objects,
        redaction_ref,
        &Redaction::new(leaked, "leaked credential"),
        Some(&forge.admin),
        250,
    );

    // Someone tries to push it back in, as part of an ordinary issue
    // mutation's object graph.
    let issue_ref = namespace::issue_ref("1").expect("valid");
    let issue = Issue {
        title: "t".into(),
        body: "b".into(),
        state: "open".into(),
        assignees: vec![],
        labels: vec![],
    };
    let tip = build_mutation(&forge.objects, &issue_ref, &issue, Some(&forge.admin), 300);

    let outcome = receive(
        &forge.refs,
        &forge.objects,
        &NullEventSink,
        &single(
            RefTransition {
                name: issue_ref.clone(),
                old: None,
                new: Some(tip),
            },
            vec![tip, leaked],
        ),
        Mode::Mandatory,
    )
    .expect("evaluates");

    assert_eq!(outcome.result, TxResult::Redacted { oid: leaked });
    assert!(
        outcome.verdicts.is_empty(),
        "refused before any verdict was evaluated"
    );
    assert!(
        forge
            .refs
            .get(issue_ref.as_ref())
            .expect("readable")
            .is_none(),
        "the whole batch must be refused, not just the redacted object"
    );
}

// ---------------------------------------------------------------------
// receive.dedup: redelivering the same (effect, oid) pair is a no-op.
// ---------------------------------------------------------------------

#[rstest]
// @relation(receive.dedup, scope=function, role=Verifies)
fn memory_sink_deduplicates_by_effect_and_oid() {
    use ents_receive::EventSink as _;

    let sink = MemoryEventSink::default();
    let oid = ObjectId::null(gix_hash::Kind::Sha1);

    sink.enqueue("unit", oid).expect("infallible");
    sink.enqueue("unit", oid).expect("infallible");
    sink.enqueue("integration", oid).expect("infallible");

    assert_eq!(
        sink.pending(),
        vec![("integration".to_owned(), oid), ("unit".to_owned(), oid),]
    );
}

// ---------------------------------------------------------------------
// receive.reconstructible: the boot-time scan rebuilds exactly the
// obligations incremental `receive` calls would have enqueued.
// ---------------------------------------------------------------------

/// Two chained empty-tree commits, built deterministically (fixed actor,
/// fixed tree, fixed seconds) so two independent object stores produce
/// byte-identical oids — letting path A and path B below compare `pending()`
/// sets directly, oids included, without sharing any state.
fn chain_commits(objects: &ObjectStore, count: usize, start_seconds: i64) -> Vec<ObjectId> {
    let tree = ents_testutil::empty_tree(objects);
    let mut parent = None;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let seconds = start_seconds.saturating_add(i64::try_from(i).unwrap_or(i64::MAX));
        let commit = write_commit(
            objects,
            &CommitSpec {
                tree,
                parents: parent.into_iter().collect(),
                message: format!("commit {i} at {seconds}"),
                seconds,
            },
            None,
        );
        parent = Some(commit);
        out.push(commit);
    }
    out
}

#[rstest]
// @relation(receive.reconstructible, receive.event-sink, receive.never-blocks, query.workset, scope=function, role=Verifies)
fn reconcile_matches_incremental_delivery() {
    let effect = Effect {
        trigger: "rev(refs/heads/main)".to_owned(),
        toolchains: vec![],
        run: "true".to_owned(),
    };
    let effect_ref: FullName = "refs/meta/effects/unit".try_into().expect("valid");
    let main = name("refs/heads/main");

    // Path A: two `receive` calls, each advancing refs/heads/main by one
    // commit — the only thing that ever moves this ref — incrementally
    // enqueuing into a live sink.
    let incremental = {
        let refs = MemRefStore::default();
        let objects = ObjectStore::default();
        write_meta_entity(&refs, &objects, effect_ref.clone(), &effect, None, 50);
        let sink = MemoryEventSink::default();

        let mut old = None;
        for commit in chain_commits(&objects, 2, 100) {
            let outcome = receive(
                &refs,
                &objects,
                &sink,
                &single(
                    RefTransition {
                        name: main.clone(),
                        old,
                        new: Some(commit),
                    },
                    vec![],
                ),
                Mode::Advisory,
            )
            .expect("evaluates");
            assert_eq!(outcome.result, TxResult::Applied);
            old = Some(commit);
        }
        sink.pending()
    };

    // Path B: an independent store, seeded directly to the same final
    // state (as if `refs/heads/main` had already advanced through two
    // accepted pushes whose enqueues were lost — a crashed in-memory
    // sink, say) — then reconstructed from repository state alone, with
    // no incremental delivery at all.
    let reconciled = {
        let refs = MemRefStore::default();
        let objects = ObjectStore::default();
        write_meta_entity(&refs, &objects, effect_ref, &effect, None, 50);
        let commits = chain_commits(&objects, 2, 100);
        refs.set(main.as_ref(), *commits.last().expect("non-empty chain"));

        let sink = MemoryEventSink::default();
        reconcile(&refs, &objects, &sink).expect("reconciles");
        sink.pending()
    };

    assert_eq!(incremental, reconciled);
    assert_eq!(
        incremental.len(),
        2,
        "one obligation per commit that entered the trigger's set"
    );
}
