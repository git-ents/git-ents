//! Integration tests for `receive`: the mandatory/advisory gate-policy
//! table (rstest — the spec's two named policies), redaction enforcement
//! (admin-only push, ingest refusal), `(effect, oid)` dedup, and the
//! reconstructibility proof that a boot-time [`reconcile`] rebuilds exactly
//! the obligations incremental `receive` calls would have enqueued.

#![expect(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "integration test: fixtures panic on setup failure"
)]

use ents_gate::Config;
use ents_model::{Effect, MemberId, Provenance, Redaction, ResultRecord, Status, namespace};
use ents_receive::{
    Identity, MemoryEventSink, Mode, NullEventSink, Proposal, RefTransition, TxResult,
    propose_genesis, receive, reconcile,
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

/// A stand-in for `ents-forge`'s `Issue` (this crate cannot depend on
/// `ents-forge`, which itself depends on this crate): any multi-field
/// entity exercises the same gate-policy and redaction-ingest machinery,
/// which is generic over the typed tree.
#[derive(Debug, Clone, PartialEq, Eq, facet::Facet)]
struct Issue {
    title: String,
    body: String,
    state: String,
}

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

/// Build a signed (or unsigned) parentless genesis commit for `entity`,
/// *without* moving any ref — unlike `ents_testutil::write_meta_entity`, so
/// the test can hand the result to `receive` and observe whether *it* moves
/// the ref. The commit names no ref (the gate recomputes the binding from
/// signed content); callers derive the refname from the returned oid where
/// the namespace is hash-identified.
fn build_mutation<T: for<'facet> facet::Facet<'facet>>(
    objects: &ObjectStore,
    entity: &T,
    signer: Option<&Keypair>,
    seconds: i64,
) -> ObjectId {
    let tree = facet_git_tree::serialize_into(entity, objects).expect("serializes");
    write_commit(
        objects,
        &CommitSpec {
            tree,
            parents: vec![],
            message: "Mutate entity".into(),
            seconds,
        },
        signer,
    )
}

/// The oid-keyed refname of a hash-identified issue genesis
/// (`meta-ref.identity-binding`).
fn issue_ref(genesis: ObjectId) -> FullName {
    name(&format!("refs/meta/issues/{genesis}"))
}

fn sample_issue() -> Issue {
    Issue {
        title: "t".into(),
        body: "b".into(),
        state: "open".into(),
    }
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
    let signer = if authorized {
        &forge.admin
    } else {
        &forge.guest
    };
    let tip = build_mutation(&forge.objects, &sample_issue(), Some(signer), 300);
    let refname = issue_ref(tip);

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
    let tip = build_mutation(&forge.objects, &redaction, Some(signer), 300);

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
    let tip = build_mutation(&forge.objects, &sample_issue(), Some(&forge.admin), 300);
    let issue_ref = issue_ref(tip);

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
        name: "unit".to_owned(),
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

// ---------------------------------------------------------------------
// model.review-pin: the retention pin's commit shape — empty tree,
// parents include the retained commit, merge-shaped fast-forward on
// every advance — admitted by the identical mandatory gate every entity
// mutation faces.
// ---------------------------------------------------------------------

/// Read a commit's `(tree, parents)` back out of the store.
fn commit_shape(objects: &ObjectStore, oid: ObjectId) -> (ObjectId, Vec<ObjectId>) {
    use gix_object::Find as _;

    let mut buf = Vec::new();
    let data = objects
        .try_find(&oid, &mut buf)
        .expect("readable")
        .expect("present");
    assert_eq!(data.kind, Kind::Commit);
    let commit = gix_object::CommitRef::from_bytes(data.data, oid.kind()).expect("parses");
    (commit.tree(), commit.parents().collect())
}

/// A first pin retains the reviewed commit as its only parent and carries
/// the empty tree; a re-review advances the pin fast-forward with a
/// merge-shaped commit `(previous pin tip, newly reviewed commit)` — and
/// the *mandatory* gate admits both shapes (`gate.tip-signed`,
/// `gate.fast-forward`: descent through any parent).
// @relation(model.review-pin, meta-ref.namespace, gate.fast-forward, scope=function, role=Verifies)
#[test]
fn pin_retains_every_reviewed_round_and_passes_the_mandatory_gate() {
    let forge = forge();
    let identity = ents_receive::Identity {
        actor: gix::actor::Signature {
            name: "admin".into(),
            email: "admin@ents.test".into(),
            time: gix::date::Time {
                seconds: 300,
                offset: 0,
            },
        },
        author: None,
        sign: &|payload| forge.admin.sign(payload),
    };
    let rounds = chain_commits(&forge.objects, 2, 250);
    let (first_round, second_round) = (rounds[0], rounds[1]);
    let pin = namespace::review_pin_ref("deadbeef", &MemberId::new("admin")).expect("valid");
    let empty = ents_testutil::empty_tree(&forge.objects);

    // First review: the pin's tip has the reviewed commit as its only
    // parent and carries no entity — the empty tree.
    let outcome = ents_receive::propose_pin(
        &forge.refs,
        &forge.objects,
        &NullEventSink,
        pin.clone(),
        first_round,
        &identity,
        "Pin review 7",
        Mode::Mandatory,
    )
    .expect("reaches an outcome");
    assert_eq!(outcome.result, TxResult::Applied);
    assert!(outcome.verdicts[0].1.is_pass(), "mandatory gate admits it");
    let first_tip = forge
        .refs
        .get(pin.as_ref())
        .expect("readable")
        .expect("set");
    let (tree, parents) = commit_shape(&forge.objects, first_tip);
    assert_eq!(tree, empty, "a pin commit carries the empty tree");
    assert_eq!(parents, vec![first_round]);

    // Re-review after the target moved: merge-shaped fast-forward
    // (previous pin tip, newly reviewed commit) — every reviewed round
    // stays retained in the pin's own history.
    let outcome = ents_receive::propose_pin(
        &forge.refs,
        &forge.objects,
        &NullEventSink,
        pin.clone(),
        second_round,
        &identity,
        "Pin review 7 again",
        Mode::Mandatory,
    )
    .expect("reaches an outcome");
    assert_eq!(outcome.result, TxResult::Applied);
    assert!(outcome.verdicts[0].1.is_pass());
    let second_tip = forge
        .refs
        .get(pin.as_ref())
        .expect("readable")
        .expect("set");
    let (tree, parents) = commit_shape(&forge.objects, second_tip);
    assert_eq!(tree, empty);
    assert_eq!(
        parents,
        vec![first_tip, second_round],
        "previous pin tip first, newly reviewed commit second"
    );
}

/// A self-attested member is not authorized for the pin namespace — the
/// gate's canonical-ref arm applies to `refs/meta/pins/*` unchanged.
// @relation(model.review-pin, gate.tip-signed, scope=function, role=Verifies)
#[test]
fn pin_writes_face_the_same_authorization_as_any_canonical_ref() {
    let forge = forge();
    let identity = ents_receive::Identity {
        actor: gix::actor::Signature {
            name: "guest".into(),
            email: "guest@ents.test".into(),
            time: gix::date::Time {
                seconds: 300,
                offset: 0,
            },
        },
        author: None,
        sign: &|payload| forge.guest.sign(payload),
    };
    let reviewed = chain_commits(&forge.objects, 1, 250)[0];
    let pin = namespace::review_pin_ref("deadbeef", &MemberId::new("admin")).expect("valid");

    let outcome = ents_receive::propose_pin(
        &forge.refs,
        &forge.objects,
        &NullEventSink,
        pin,
        reviewed,
        &identity,
        "Pin review 7",
        Mode::Mandatory,
    )
    .expect("reaches an outcome");
    assert_eq!(outcome.result, TxResult::Refused);
}

// ---------------------------------------------------------------------
// Identity binding driven through receive: the replays the binding closes.
// ---------------------------------------------------------------------

fn comment_ref(genesis: ObjectId) -> FullName {
    name(&format!("refs/meta/comments/{genesis}"))
}

/// The doppelgänger replay: a signed *mutation* commit (one with a parent)
/// re-proposed as the genesis of a fresh entity is refused, because the
/// all-roots walk reaches the original genesis, not the replayed commit
/// (`gate.identity-binding`, `meta-ref.identity-binding`). This, not a
/// creation-time-only check, is what makes the replay impossible.
// @relation(gate.identity-binding, meta-ref.identity-binding, scope=function, role=Verifies)
#[test]
fn a_signed_mutation_replayed_as_a_fresh_genesis_is_refused() {
    let forge = forge();
    // A legitimate comment genesis and one signed mutation of it.
    let genesis = build_mutation(&forge.objects, &sample_issue(), Some(&forge.admin), 300);
    forge.refs.set(comment_ref(genesis).as_ref(), genesis);
    let mut edited = sample_issue();
    edited.state = "resolved".into();
    let tree = facet_git_tree::serialize_into(&edited, &forge.objects).expect("ser");
    let mutation = write_commit(
        &forge.objects,
        &CommitSpec {
            tree,
            parents: vec![genesis],
            message: "Resolve".into(),
            seconds: 310,
        },
        Some(&forge.admin),
    );

    // Re-propose that mutation commit as the genesis of a brand-new
    // comment named for its own oid.
    let outcome = receive(
        &forge.refs,
        &forge.objects,
        &NullEventSink,
        &single(
            RefTransition {
                name: comment_ref(mutation),
                old: None,
                new: Some(mutation),
            },
            vec![mutation],
        ),
        Mode::Mandatory,
    )
    .expect("evaluates");

    assert_eq!(outcome.result, TxResult::Refused);
    let (_, verdict) = &outcome.verdicts[0];
    let ents_gate::Verdict::Fail(refusal) = verdict else {
        panic!("the replay must be refused: {verdict:?}");
    };
    assert_eq!(refusal.requirement, ents_gate::Requirement::IdentityBinding);
    assert!(
        forge
            .refs
            .get(comment_ref(mutation).as_ref())
            .expect("readable")
            .is_none(),
        "no doppelgänger entity was created"
    );
}

/// The result replay: a signed `pass` re-proposed for a different effect,
/// or a different commit, is refused — the result tree carries its own
/// effect and target, so the refname is a function of signed content
/// (`model.result-identity`, `gate.identity-binding`).
// @relation(model.result-identity, gate.identity-binding, scope=function, role=Verifies)
#[rstest]
#[case::wrong_effect("lint", "abc123")]
#[case::wrong_commit("unit", "ffffff")]
fn a_signed_pass_replayed_for_another_effect_or_commit_is_refused(
    #[case] effect_seg: &str,
    #[case] short: &str,
) {
    let forge = forge();
    let target =
        ObjectId::from_hex(b"abc1230000000000000000000000000000000000").expect("valid hex");
    let record = ResultRecord::new("unit", target, Status::Pass);
    let tip = build_mutation(&forge.objects, &record, Some(&forge.admin), 300);

    // The honest ref is results/unit/abc123; the replay targets a
    // different effect or commit segment.
    let replay = namespace::result_ref(effect_seg, short).expect("valid");
    let outcome = receive(
        &forge.refs,
        &forge.objects,
        &NullEventSink,
        &single(
            RefTransition {
                name: replay.clone(),
                old: None,
                new: Some(tip),
            },
            vec![tip],
        ),
        Mode::Mandatory,
    )
    .expect("evaluates");

    assert_eq!(outcome.result, TxResult::Refused);
    assert!(forge.refs.get(replay.as_ref()).expect("readable").is_none());
}

/// Creation via the inbox still works: a self-attested member creates a
/// hash-identified entity under its own inbox segment, awaiting adoption
/// (`gate.owner-mutation`: creation stays provenance-keyed). The
/// sign-then-name genesis flow names the ref from the commit's own oid.
// @relation(gate.owner-mutation, meta-ref.inbox, meta-ref.identity-binding, scope=function, role=Verifies)
#[test]
fn a_self_attested_member_creates_a_comment_in_its_inbox() {
    let forge = forge();
    let identity = Identity {
        actor: gix::actor::Signature {
            name: "guest".into(),
            email: "guest@ents.test".into(),
            time: gix::date::Time {
                seconds: 300,
                offset: 0,
            },
        },
        author: None,
        sign: &|payload| forge.guest.sign(payload),
    };

    let (landed, outcome) = propose_genesis(
        &forge.refs,
        &forge.objects,
        &NullEventSink,
        &sample_issue(),
        |oid| namespace::inbox_ref(&MemberId::new("guest"), &format!("comments/{oid}")),
        &identity,
        "Comment awaiting adoption",
        Mode::Mandatory,
    )
    .expect("reaches an outcome");

    assert_eq!(outcome.result, TxResult::Applied, "{:?}", outcome.verdicts);
    assert!(
        landed
            .as_bstr()
            .starts_with(b"refs/meta/inbox/guest/comments/"),
        "created under the contributor's own inbox segment: {landed}"
    );
    assert!(forge.refs.get(landed.as_ref()).expect("readable").is_some());
}

/// An attributed mutation ("member via the web") carries the attributed
/// member in the commit's author slot while the committer — and the
/// signature the gate judges — stays the signing identity; the mandatory
/// gate admits it exactly as it would the unattributed form
/// (`receive.attributed-author`).
// @relation(receive.attributed-author, scope=function, role=Verifies)
#[test]
fn an_attributed_author_lands_in_the_author_slot_and_the_gate_judges_the_signer() {
    use gix_object::Find as _;

    let forge = forge();
    let identity = Identity {
        actor: gix::actor::Signature {
            name: "admin".into(),
            email: "admin@ents.test".into(),
            time: gix::date::Time {
                seconds: 300,
                offset: 0,
            },
        },
        author: Some(gix::actor::Signature {
            name: "guest".into(),
            email: "guest@ents.test".into(),
            time: gix::date::Time {
                seconds: 290,
                offset: 0,
            },
        }),
        sign: &|payload| forge.admin.sign(payload),
    };
    let refname = namespace::redaction_ref("r-attr").expect("valid");
    let redaction = Redaction::new(ObjectId::null(gix_hash::Kind::Sha1), "leaked credential");

    let outcome = ents_receive::propose_entity(
        &forge.refs,
        &forge.objects,
        &NullEventSink,
        refname.clone(),
        &redaction,
        &identity,
        "Redact, attributed",
        Mode::Mandatory,
    )
    .expect("reaches an outcome");
    assert_eq!(outcome.result, TxResult::Applied, "{:?}", outcome.verdicts);

    let tip = forge
        .refs
        .get(refname.as_ref())
        .expect("readable")
        .expect("written");
    let mut buf = Vec::new();
    let data = forge
        .objects
        .try_find(&tip, &mut buf)
        .expect("readable")
        .expect("present");
    let commit = gix_object::CommitRef::from_bytes(data.data, tip.kind()).expect("parses");
    let author = commit.author().expect("author parses");
    let committer = commit.committer().expect("committer parses");
    assert_eq!(author.name, "guest", "attributed author");
    assert_eq!(committer.name, "admin", "signing committer");
}
