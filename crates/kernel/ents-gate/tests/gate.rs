//! Integration tests for the gate: the verdict table (rstest — the spec
//! enumerates the cases), identity binding and owner mutation, the epoch
//! and bootstrap windows, and the one-parameterized-test proof that all
//! three call sites see identical verdicts.

#![expect(
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    reason = "integration test: fixtures panic on setup failure"
)]

use ents_anchor::Binding;
use ents_gate::{AdmissionKind, Config, Requirement, Update, Verdict, verify};
use ents_model::{
    Claim, Effect, Member, MemberId, Provenance, ResultRecord, Status,
    claim::Verdict as ClaimVerdict, namespace,
};
use ents_testutil::{
    CommitSpec, Keypair, MemRefStore, ObjectStore, empty_tree, enroll_member, write_commit,
    write_member, write_meta_entity,
};
use gix::refs::FullName;
use gix_hash::ObjectId;
use gix_ref_store::{Expected, RefStoreRead as _};
use rstest::rstest;

const ADMIN_SEED: u8 = 1;
const SELF_ATTESTED_SEED: u8 = 2;
const OUTSIDER_SEED: u8 = 9;

/// A forge fixture with verification in force: an admin-registered
/// member `admin` (enrolled pre-epoch), a self-attested member `guest`,
/// and an epoch recorded in `refs/meta/config`.
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
    let guest = Keypair::from_seed(SELF_ATTESTED_SEED);
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

/// The oid-keyed refname of a hash-identified issue whose genesis is
/// `genesis` (`meta-ref.identity-binding`).
fn issue_ref(genesis: ObjectId) -> FullName {
    name(&format!("refs/meta/issues/{genesis}"))
}

/// A signed empty-tree commit — a generic meta-mutation body. A
/// hash-identified entity binds by its genesis oid and the all-roots
/// walk, not by tree content, so an empty tree exercises the tip
/// invariant on `refs/meta/issues/*` and `refs/meta/comments/*` fully.
fn commit(forge: &Forge, parents: Vec<ObjectId>, key: Option<&Keypair>, seconds: i64) -> ObjectId {
    let tree = empty_tree(&forge.objects);
    write_commit(
        &forge.objects,
        &CommitSpec {
            tree,
            parents,
            message: "mutate".into(),
            seconds,
        },
        key,
    )
}

/// A signed commit whose tree is `tree` — for the namespaces whose binding
/// reads a tree field (natural-key, composite).
fn tree_commit(
    forge: &Forge,
    tree: ObjectId,
    parents: Vec<ObjectId>,
    key: Option<&Keypair>,
    seconds: i64,
) -> ObjectId {
    write_commit(
        &forge.objects,
        &CommitSpec {
            tree,
            parents,
            message: "mutate".into(),
            seconds,
        },
        key,
    )
}

fn run(forge: &Forge, refname: &FullName, new: Option<ObjectId>) -> Verdict {
    verify(
        &forge.refs,
        &forge.objects,
        &Update {
            name: refname.clone(),
            new,
        },
    )
    .expect("the gate must reach a verdict on a complete fixture")
}

fn expect_fail(verdict: &Verdict, requirement: Requirement) {
    let Verdict::Fail(refusal) = verdict else {
        panic!("expected a refusal against {requirement:?}, got {verdict:?}");
    };
    assert_eq!(refusal.requirement, requirement, "refusal: {refusal}");
}

fn expect_pass(verdict: &Verdict, kind: AdmissionKind) {
    let Verdict::Pass(admission) = verdict else {
        panic!("expected admission {kind:?}, got {verdict:?}");
    };
    assert_eq!(admission.kind, kind);
}

// ---------------------------------------------------------------------
// The verdict table: member × signature × FF × namespace cases.
// ---------------------------------------------------------------------

#[rstest]
// @relation(gate.tip-signed, gate.verdict-reason, scope=function, role=Verifies)
fn authorized_signed_mutation_passes_the_tip_invariant() {
    let f = forge();
    let new = commit(&f, vec![], Some(&f.admin), 300);
    expect_pass(
        &run(&f, &issue_ref(new), Some(new)),
        AdmissionKind::TipInvariant,
    );
}

#[rstest]
// @relation(gate.tip-signed, scope=function, role=Verifies)
fn unsigned_tip_is_refused() {
    let f = forge();
    let new = commit(&f, vec![], None, 300);
    expect_fail(&run(&f, &issue_ref(new), Some(new)), Requirement::TipSigned);
}

#[rstest]
// @relation(gate.tip-signed, scope=function, role=Verifies)
fn non_member_signature_is_refused() {
    let f = forge();
    let outsider = Keypair::from_seed(OUTSIDER_SEED);
    let new = commit(&f, vec![], Some(&outsider), 300);
    expect_fail(&run(&f, &issue_ref(new), Some(new)), Requirement::TipSigned);
}

/// Revoke `id`'s key in the fixture, as an admin-signed mutation of the
/// member's ref.
fn revoke(f: &Forge, id: &str, key: &Keypair, provenance: Provenance, seconds: i64) {
    let mut revoked = Member::new(id, key.public_openssh(), provenance);
    revoked.revoke();
    write_member(&f.refs, &f.objects, id, &revoked, Some(&f.admin), seconds);
}

#[rstest]
// @relation(gate.tip-signed, model.member-revocation, scope=function, role=Verifies)
fn a_revoked_members_new_push_is_refused() {
    let f = forge();
    revoke(&f, "admin", &f.admin, Provenance::AdminRegistered, 400);

    let new = commit(&f, vec![], Some(&f.admin), 500);
    let verdict = run(&f, &issue_ref(new), Some(new));
    expect_fail(&verdict, Requirement::TipSigned);
    let Verdict::Fail(refusal) = &verdict else {
        unreachable!()
    };
    assert!(refusal.detail.contains("revoked"), "detail: {refusal}");
}

#[rstest]
// @relation(model.member-revocation, gate.tip-signed, scope=function, role=Verifies)
fn a_backdated_commit_cannot_reach_past_a_revocation() {
    // The security-review regression: admission consults the member
    // entity currently in force, so a revoked key authoring a NEW commit
    // with a committer timestamp claimed from before the revocation —
    // descending cleanly from the live tip — is still refused.
    let f = forge();
    let genesis = commit(&f, vec![], Some(&f.admin), 300);
    let refname = issue_ref(genesis);
    f.refs.set(refname.as_ref(), genesis);
    revoke(&f, "admin", &f.admin, Provenance::AdminRegistered, 400);

    // Authored "at 300", pushed after the revocation at 400.
    let backdated = commit(&f, vec![genesis], Some(&f.admin), 300);
    expect_fail(&run(&f, &refname, Some(backdated)), Requirement::TipSigned);
}

#[rstest]
// @relation(model.member-revocation, gate.fast-forward, scope=function, role=Verifies)
fn refs_accepted_before_a_revocation_are_never_rejudged() {
    // A second admin keeps working on a ref whose current tip was placed
    // by a member revoked afterwards: the accepted tip stays valid
    // history, and fast-forwarding over it is ordinary descent.
    let f = forge();
    let second = Keypair::from_seed(OUTSIDER_SEED);
    enroll_member(
        &f.refs,
        &f.objects,
        "second",
        &second,
        Provenance::AdminRegistered,
        310,
    );

    let genesis = commit(&f, vec![], Some(&f.admin), 320);
    let refname = issue_ref(genesis);
    f.refs.set(refname.as_ref(), genesis);
    revoke(&f, "admin", &f.admin, Provenance::AdminRegistered, 400);

    let continued = commit(&f, vec![genesis], Some(&second), 500);
    expect_pass(
        &run(&f, &refname, Some(continued)),
        AdmissionKind::TipInvariant,
    );
}

#[rstest]
// @relation(gate.identity-binding, meta-ref.identity-binding, scope=function, role=Verifies)
fn a_refname_not_naming_the_genesis_oid_is_refused() {
    // The identity binding replaces the retired Advance-ref trailer: a
    // signed commit proposed under a refname whose final segment is not
    // its genesis oid is refused, so it cannot be replayed as the tip of a
    // different meta-ref than the one its content names.
    let f = forge();
    let genesis = commit(&f, vec![], Some(&f.admin), 300);
    // A different, valid oid that is not this commit's genesis.
    let wrong =
        issue_ref(ObjectId::from_hex(b"00000000000000000000000000000000deadbeef").expect("hex"));
    expect_fail(
        &run(&f, &wrong, Some(genesis)),
        Requirement::IdentityBinding,
    );
}

#[rstest]
// @relation(gate.fast-forward, scope=function, role=Verifies)
fn non_fast_forward_is_refused() {
    let f = forge();
    // A genesis and two children of it, all correctly bound to the same
    // oid-keyed ref; the current tip is one child, the proposal the
    // sibling — a genuine fast-forward refusal, not an identity mismatch.
    let genesis = commit(&f, vec![], Some(&f.admin), 300);
    let refname = issue_ref(genesis);
    let current = commit(&f, vec![genesis], Some(&f.admin), 310);
    let sibling = commit(&f, vec![genesis], Some(&f.admin), 311);
    f.refs.set(refname.as_ref(), current);
    assert_ne!(sibling, current);
    expect_fail(&run(&f, &refname, Some(sibling)), Requirement::FastForward);
}

#[rstest]
// @relation(gate.fast-forward, scope=function, role=Verifies)
fn meta_ref_deletion_is_refused() {
    let f = forge();
    let genesis = commit(&f, vec![], Some(&f.admin), 300);
    let refname = issue_ref(genesis);
    f.refs.set(refname.as_ref(), genesis);
    expect_fail(&run(&f, &refname, None), Requirement::FastForward);
}

#[rstest]
// @relation(gate.principled-split, scope=function, role=Verifies)
fn code_refs_are_not_subject_to_the_tip_invariant() {
    let f = forge();
    // Unsigned, non-FF — none of it matters outside refs/meta/*.
    let new = commit(&f, vec![], None, 300);
    expect_pass(
        &run(&f, &name("refs/heads/main"), Some(new)),
        AdmissionKind::CodeRef,
    );
}

// ---------------------------------------------------------------------
// Identity binding: the refname is a function of signed content.
// ---------------------------------------------------------------------

#[rstest]
// @relation(gate.identity-binding, meta-ref.identity-binding, model.member-identity, scope=function, role=Verifies)
fn a_natural_key_member_binds_by_its_id_field() {
    let f = forge();
    // A member entity whose id field disagrees with the refname's final
    // segment is refused; agreeing, it passes.
    let mismatched = Member::new(
        "someone-else",
        f.admin.public_openssh(),
        Provenance::AdminRegistered,
    );
    let tree = facet_git_tree::serialize_into(&mismatched, &f.objects).expect("ser");
    let tip = tree_commit(&f, tree, vec![], Some(&f.admin), 300);
    expect_fail(
        &run(&f, &name("refs/meta/member/newcomer"), Some(tip)),
        Requirement::IdentityBinding,
    );

    let matched = Member::new(
        "newcomer",
        f.admin.public_openssh(),
        Provenance::AdminRegistered,
    );
    let tree = facet_git_tree::serialize_into(&matched, &f.objects).expect("ser");
    let tip = tree_commit(&f, tree, vec![], Some(&f.admin), 300);
    expect_pass(
        &run(&f, &name("refs/meta/member/newcomer"), Some(tip)),
        AdmissionKind::TipInvariant,
    );
}

#[rstest]
// @relation(gate.identity-binding, meta-ref.identity-binding, model.effect-definition, scope=function, role=Verifies)
fn a_natural_key_effect_binds_by_its_name_field() {
    let f = forge();
    let effect = Effect {
        name: "unit".into(),
        trigger: "rev(refs/heads/main)".into(),
        toolchains: vec![],
        run: "true".into(),
    };
    let tree = facet_git_tree::serialize_into(&effect, &f.objects).expect("ser");
    // Named `unit` in the tree — refused under a ref that names `lint`.
    let tip = tree_commit(&f, tree, vec![], Some(&f.admin), 300);
    expect_fail(
        &run(&f, &name("refs/meta/effects/lint"), Some(tip)),
        Requirement::IdentityBinding,
    );
    // Under the ref its name field recomputes, it passes.
    expect_pass(
        &run(&f, &name("refs/meta/effects/unit"), Some(tip)),
        AdmissionKind::TipInvariant,
    );
}

/// A commit recording `result` at `results/<effect>/<short>`.
fn result_commit(
    f: &Forge,
    effect: &str,
    target: ObjectId,
    parents: Vec<ObjectId>,
    key: Option<&Keypair>,
    seconds: i64,
) -> ObjectId {
    let record = ResultRecord::new(effect, target, Status::Pass);
    let tree = facet_git_tree::serialize_into(&record, &f.objects).expect("ser");
    tree_commit(f, tree, parents, key, seconds)
}

#[rstest]
// @relation(gate.identity-binding, model.result-identity, scope=function, role=Verifies)
fn a_composite_result_binds_by_its_effect_and_target_fields() {
    let f = forge();
    let target = ObjectId::from_hex(b"abc1230000000000000000000000000000000000").expect("hex");
    let tip = result_commit(&f, "unit", target, vec![], Some(&f.admin), 300);

    // Correct effect and short-oid prefix: passes.
    expect_pass(
        &run(
            &f,
            &namespace::result_ref("unit", "abc123").expect("valid"),
            Some(tip),
        ),
        AdmissionKind::TipInvariant,
    );
    // Wrong effect segment: refused.
    expect_fail(
        &run(
            &f,
            &namespace::result_ref("lint", "abc123").expect("valid"),
            Some(tip),
        ),
        Requirement::IdentityBinding,
    );
    // Short oid that is not a prefix of the target field: refused.
    expect_fail(
        &run(
            &f,
            &namespace::result_ref("unit", "ffffff").expect("valid"),
            Some(tip),
        ),
        Requirement::IdentityBinding,
    );
}

/// A stand-in Review tree: the gate reads only the `target` field
/// generically, so any struct carrying it exercises the composite key.
#[derive(facet::Facet)]
struct Review {
    target: [u8; 20],
    verdict: String,
}

fn review_commit(f: &Forge, target: ObjectId, key: Option<&Keypair>, seconds: i64) -> ObjectId {
    let mut bytes = [0u8; 20];
    bytes.copy_from_slice(target.as_slice());
    let review = Review {
        target: bytes,
        verdict: "approve".into(),
    };
    let tree = facet_git_tree::serialize_into(&review, &f.objects).expect("ser");
    tree_commit(f, tree, vec![], key, seconds)
}

#[rstest]
// @relation(gate.identity-binding, model.review, scope=function, role=Verifies)
fn a_composite_review_binds_by_its_target_field_and_signer() {
    let f = forge();
    let target = ObjectId::from_hex(b"deadbeef00000000000000000000000000000000").expect("hex");
    let tip = review_commit(&f, target, Some(&f.admin), 300);
    let good = namespace::review_ref(&target.to_string(), &MemberId::new("admin")).expect("valid");
    expect_pass(&run(&f, &good, Some(tip)), AdmissionKind::TipInvariant);

    // Right target, wrong reviewer segment: the signer is admin, not guest.
    let wrong_member =
        namespace::review_ref(&target.to_string(), &MemberId::new("guest")).expect("valid");
    expect_fail(
        &run(&f, &wrong_member, Some(tip)),
        Requirement::IdentityBinding,
    );

    // Right reviewer, wrong target segment.
    let other = ObjectId::from_hex(b"0000000000000000000000000000000000000001").expect("hex");
    let wrong_target =
        namespace::review_ref(&other.to_string(), &MemberId::new("admin")).expect("valid");
    expect_fail(
        &run(&f, &wrong_target, Some(tip)),
        Requirement::IdentityBinding,
    );
}

/// A result tree carrying an entry that is not a `ResultRecord` field —
/// for the strict-decode disjointness check.
#[derive(facet::Facet)]
struct ResultPlus {
    effect: String,
    target: [u8; 20],
    status: Status,
    surprise: String,
}

#[rstest]
// @relation(gate.identity-binding, meta-ref.typed-tree, scope=function, role=Verifies)
fn strict_genesis_decode_refuses_an_unknown_tree_entry() {
    let f = forge();
    let target = ObjectId::from_hex(b"abc1230000000000000000000000000000000000").expect("hex");
    let mut bytes = [0u8; 20];
    bytes.copy_from_slice(target.as_slice());
    let bogus = ResultPlus {
        effect: "unit".into(),
        target: bytes,
        status: Status::Pass,
        surprise: "not a result field".into(),
    };
    let tree = facet_git_tree::serialize_into(&bogus, &f.objects).expect("ser");
    let tip = tree_commit(&f, tree, vec![], Some(&f.admin), 300);
    // Even though effect and short-oid recompute, the extra tree entry
    // makes strict decode refuse the genesis.
    expect_fail(
        &run(
            &f,
            &namespace::result_ref("unit", "abc123").expect("valid"),
            Some(tip),
        ),
        Requirement::IdentityBinding,
    );
}

#[rstest]
// @relation(gate.identity-binding, gate.same-actor-divergence, scope=function, role=Verifies)
fn the_all_roots_walk_holds_across_a_sync_created_merge() {
    // Two children of the same genesis, merged: the merge has two parents
    // but a single parentless root (the genesis), so the hash-identified
    // binding still recomputes the genesis oid across the merge commit.
    let f = forge();
    let genesis = commit(&f, vec![], Some(&f.admin), 300);
    let refname = issue_ref(genesis);
    let a = commit(&f, vec![genesis], Some(&f.admin), 310);
    let b = commit(&f, vec![genesis], Some(&f.admin), 311);
    f.refs.set(refname.as_ref(), a);
    let merge = commit(&f, vec![a, b], Some(&f.admin), 320);
    expect_pass(&run(&f, &refname, Some(merge)), AdmissionKind::TipInvariant);

    // The same merge proposed under a doppelgänger genesis id (the merge's
    // own oid) is refused: its parentless root is still the genesis.
    expect_fail(
        &run(&f, &issue_ref(merge), Some(merge)),
        Requirement::IdentityBinding,
    );
}

#[rstest]
// @relation(gate.identity-binding, model.review-pin, scope=function, role=Verifies)
fn a_pin_is_never_subjected_to_the_all_roots_walk() {
    // A pin's ancestry reaches into code history (its parents include the
    // reviewed commit), so the parentless-roots walk must not apply. A pin
    // commit whose parent is an arbitrary reviewed commit — not a genesis
    // whose oid is the pin's id — still binds.
    let f = forge();
    let reviewed = commit(&f, vec![], Some(&f.admin), 250);
    let target = reviewed.to_string();
    let pin_ref = namespace::review_pin_ref(&target, &MemberId::new("admin")).expect("valid");
    // The pin's tip is a signed commit whose parents include the reviewed
    // commit; its tree is empty. The all-roots walk would reject it (the
    // root is `reviewed`, not the pin's segments), so this passing verdict
    // proves the walk is skipped for pins.
    let pin_tip = commit(&f, vec![reviewed], Some(&f.admin), 300);
    expect_pass(
        &run(&f, &pin_ref, Some(pin_tip)),
        AdmissionKind::TipInvariant,
    );
}

// ---------------------------------------------------------------------
// Claims: append-once, witness-retaining, signer-bound genesis refs.
// ---------------------------------------------------------------------

/// A real serialized [`Claim`] tree over a `Binding::Commit { commit: witness
/// }`, asserted by `signer_id` — built through `Claim::new` so these tests
/// exercise the entity as it is actually stored, not a hand-wired tree.
fn claim_tree(f: &Forge, signer_id: &str, verdict: ClaimVerdict, witness: ObjectId) -> ObjectId {
    let binding = Binding::Commit { commit: witness };
    let claim = Claim::new(
        MemberId::new(signer_id),
        &binding,
        verdict,
        "review",
        &f.objects,
    )
    .expect("claim serializes");
    facet_git_tree::serialize_into(&claim, &f.objects).expect("ser")
}

#[rstest]
// @relation(gate.identity-binding, meta-ref.identity-binding, scope=function, role=Verifies)
fn a_claim_genesis_with_a_witness_parent_passes_the_tip_invariant() {
    let f = forge();
    let witness = commit(&f, vec![], Some(&f.admin), 250);
    let tree = claim_tree(&f, "admin", ClaimVerdict::Affirm, witness);
    let tip = tree_commit(&f, tree, vec![witness], Some(&f.admin), 300);
    let refname = namespace::claim_ref(&tip.to_string()).expect("valid");
    expect_pass(&run(&f, &refname, Some(tip)), AdmissionKind::TipInvariant);
}

#[rstest]
// @relation(gate.identity-binding, meta-ref.identity-binding, scope=function, role=Verifies)
fn a_claim_refname_not_naming_the_tips_own_oid_is_refused() {
    let f = forge();
    let witness = commit(&f, vec![], Some(&f.admin), 250);
    let tree = claim_tree(&f, "admin", ClaimVerdict::Affirm, witness);
    let tip = tree_commit(&f, tree, vec![witness], Some(&f.admin), 300);
    // Named for the witness rather than the claim's own genesis oid.
    let wrong = namespace::claim_ref(&witness.to_string()).expect("valid");
    expect_fail(&run(&f, &wrong, Some(tip)), Requirement::IdentityBinding);
}

#[rstest]
// @relation(gate.identity-binding, meta-ref.identity-binding, scope=function, role=Verifies)
fn a_claim_ref_is_append_once_and_refuses_an_advance() {
    let f = forge();
    let witness = commit(&f, vec![], Some(&f.admin), 250);
    let tree = claim_tree(&f, "admin", ClaimVerdict::Affirm, witness);
    let genesis = tree_commit(&f, tree, vec![witness], Some(&f.admin), 300);
    let refname = namespace::claim_ref(&genesis.to_string()).expect("valid");
    f.refs.set(refname.as_ref(), genesis);

    // A changed assertion is a new claim, never an advance of this one:
    // even a well-formed, correctly signed child commit under the same
    // ref is refused, because its own oid is not the ref's segment.
    let advance_tree = claim_tree(&f, "admin", ClaimVerdict::Deny, witness);
    let advance = tree_commit(&f, advance_tree, vec![genesis], Some(&f.admin), 310);
    expect_fail(
        &run(&f, &refname, Some(advance)),
        Requirement::IdentityBinding,
    );
}

#[rstest]
// @relation(gate.identity-binding, meta-ref.identity-binding, scope=function, role=Verifies)
fn a_parentless_claim_tip_is_refused() {
    let f = forge();
    let witness = commit(&f, vec![], Some(&f.admin), 250);
    let tree = claim_tree(&f, "admin", ClaimVerdict::Affirm, witness);
    let tip = tree_commit(&f, tree, vec![], Some(&f.admin), 300);
    let refname = namespace::claim_ref(&tip.to_string()).expect("valid");
    expect_fail(&run(&f, &refname, Some(tip)), Requirement::IdentityBinding);
}

#[rstest]
// @relation(gate.identity-binding, meta-ref.identity-binding, scope=function, role=Verifies)
fn a_claim_signer_field_mismatching_the_actual_signer_is_refused() {
    let f = forge();
    let witness = commit(&f, vec![], Some(&f.admin), 250);
    // The claim's tree names a signer other than whoever actually signed
    // the ledger commit.
    let tree = claim_tree(&f, "someone-else", ClaimVerdict::Affirm, witness);
    let tip = tree_commit(&f, tree, vec![witness], Some(&f.admin), 300);
    let refname = namespace::claim_ref(&tip.to_string()).expect("valid");
    expect_fail(&run(&f, &refname, Some(tip)), Requirement::IdentityBinding);
}

/// A claim tree carrying an entry that is not a `Claim` field — for the
/// strict-decode disjointness check.
#[derive(facet::Facet)]
struct ClaimPlus {
    signer: MemberId,
    binding: facet_git_tree::RawTree,
    verdict: ClaimVerdict,
    kind: String,
    surprise: String,
}

#[rstest]
// @relation(gate.identity-binding, meta-ref.typed-tree, scope=function, role=Verifies)
fn strict_genesis_decode_refuses_an_unknown_claim_tree_entry() {
    let f = forge();
    let witness = commit(&f, vec![], Some(&f.admin), 250);
    let binding = Binding::Commit { commit: witness };
    let binding_tree = binding
        .serialize_into(&f.objects)
        .expect("binding serializes");
    let bogus = ClaimPlus {
        signer: MemberId::new("admin"),
        binding: facet_git_tree::RawTree::new(binding_tree),
        verdict: ClaimVerdict::Affirm,
        kind: "review".into(),
        surprise: "not a claim field".into(),
    };
    let tree = facet_git_tree::serialize_into(&bogus, &f.objects).expect("ser");
    let tip = tree_commit(&f, tree, vec![witness], Some(&f.admin), 300);
    let refname = namespace::claim_ref(&tip.to_string()).expect("valid");
    expect_fail(&run(&f, &refname, Some(tip)), Requirement::IdentityBinding);
}

#[rstest]
// @relation(model.member-provenance, meta-ref.inbox, gate.tip-signed, scope=function, role=Verifies)
fn a_self_attested_member_falls_back_to_its_inbox_for_a_claim() {
    let f = forge();
    let witness = commit(&f, vec![], Some(&f.guest), 250);
    let tree = claim_tree(&f, "guest", ClaimVerdict::Note, witness);
    let tip = tree_commit(&f, tree, vec![witness], Some(&f.guest), 300);

    // The canonical claim ref is refused — self-attested provenance is not
    // authorized for canonical refs — with the inbox alternative surfaced.
    let canonical = namespace::claim_ref(&tip.to_string()).expect("valid");
    let verdict = run(&f, &canonical, Some(tip));
    expect_fail(&verdict, Requirement::TipSigned);
    let Verdict::Fail(refusal) = &verdict else {
        unreachable!()
    };
    assert!(refusal.inbox_alternative, "detail: {refusal}");

    // The identical claim under the member's own inbox segment passes: the
    // inbox arm recurses into the synthesized canonical refname
    // (`refs/meta/claims/<id>`) and finds the same binding.
    let inbox =
        namespace::inbox_ref(&MemberId::new("guest"), &format!("claims/{tip}")).expect("valid");
    expect_pass(&run(&f, &inbox, Some(tip)), AdmissionKind::TipInvariant);
}

#[rstest]
// @relation(meta-ref.inbox, gate.tip-signed, scope=function, role=Verifies)
fn an_inbox_claim_by_its_owner_with_a_correct_binding_passes() {
    let f = forge();
    let witness = commit(&f, vec![], Some(&f.admin), 250);
    let tree = claim_tree(&f, "admin", ClaimVerdict::Affirm, witness);
    let tip = tree_commit(&f, tree, vec![witness], Some(&f.admin), 300);
    let inbox =
        namespace::inbox_ref(&MemberId::new("admin"), &format!("claims/{tip}")).expect("valid");
    expect_pass(&run(&f, &inbox, Some(tip)), AdmissionKind::TipInvariant);
}

// ---------------------------------------------------------------------
// Owner mutation: an advance is keyed to ownership.
// ---------------------------------------------------------------------

#[rstest]
// @relation(gate.owner-mutation, scope=function, role=Verifies)
fn an_admin_may_advance_another_members_hash_identified_entity() {
    // Ownership of a hash-identified entity is intrinsic to its id, but an
    // admin-registered member may advance it too (∪ admins). A second
    // admin advances the first admin's comment.
    let f = forge();
    let second = Keypair::from_seed(OUTSIDER_SEED);
    enroll_member(
        &f.refs,
        &f.objects,
        "second",
        &second,
        Provenance::AdminRegistered,
        210,
    );

    let genesis = commit(&f, vec![], Some(&f.admin), 300);
    let refname = name(&format!("refs/meta/comments/{genesis}"));
    f.refs.set(refname.as_ref(), genesis);
    let advance = commit(&f, vec![genesis], Some(&second), 310);
    expect_pass(
        &run(&f, &refname, Some(advance)),
        AdmissionKind::TipInvariant,
    );
}

#[rstest]
// @relation(gate.owner-mutation, model.member-provenance, scope=function, role=Verifies)
fn a_self_attested_non_owner_cannot_advance_a_comment() {
    // A self-attested member is not authorized for canonical refs at all
    // (creation stays provenance-keyed, routed to the inbox); it therefore
    // cannot advance someone else's comment either.
    let f = forge();
    let genesis = commit(&f, vec![], Some(&f.admin), 300);
    let refname = name(&format!("refs/meta/comments/{genesis}"));
    f.refs.set(refname.as_ref(), genesis);
    let advance = commit(&f, vec![genesis], Some(&f.guest), 310);
    let verdict = run(&f, &refname, Some(advance));
    expect_fail(&verdict, Requirement::TipSigned);
}

#[rstest]
// @relation(gate.owner-mutation, model.review, scope=function, role=Verifies)
fn the_wrong_member_cannot_advance_a_review() {
    // A review advances only under the member its refname names. `guest`
    // (were it registered) could not advance admin's review; here we prove
    // the composite key + owner rule refuse a mismatched signer.
    let f = forge();
    let second = Keypair::from_seed(OUTSIDER_SEED);
    enroll_member(
        &f.refs,
        &f.objects,
        "second",
        &second,
        Provenance::AdminRegistered,
        210,
    );
    let target = ObjectId::from_hex(b"deadbeef00000000000000000000000000000000").expect("hex");
    // A review ref named for admin, but signed by `second`.
    let tip = review_commit(&f, target, Some(&second), 300);
    let refname =
        namespace::review_ref(&target.to_string(), &MemberId::new("admin")).expect("valid");
    let verdict = run(&f, &refname, Some(tip));
    let Verdict::Fail(_) = verdict else {
        panic!(
            "a review signed by a member other than the one it names must be refused: {verdict:?}"
        );
    };
}

// ---------------------------------------------------------------------
// Provenance-keyed authorization.
// ---------------------------------------------------------------------

#[rstest]
#[case::canonical_issue("refs/meta/issues/1", true)]
#[case::canonical_result("refs/meta/results/unit/abc", true)]
#[case::effects_is_admin_only("refs/meta/effects/unit", true)]
#[case::another_self_run("refs/meta/self/admin/unit/abc", false)]
// @relation(model.member-provenance, effect.admin-only, gate.tip-signed, scope=function, role=Verifies)
fn self_attested_members_are_limited_to_their_own_namespaces(
    #[case] refname: &str,
    #[case] inbox_alternative: bool,
) {
    // Authorization is judged before identity binding, so an empty-tree
    // commit under any of these canonical refs is refused on provenance.
    let f = forge();
    let new = commit(&f, vec![], Some(&f.guest), 300);
    let verdict = run(&f, &name(refname), Some(new));
    expect_fail(&verdict, Requirement::TipSigned);
    let Verdict::Fail(refusal) = &verdict else {
        unreachable!()
    };
    assert_eq!(
        refusal.inbox_alternative, inbox_alternative,
        "inbox hint for {refname}: {refusal}"
    );
}

#[rstest]
// @relation(meta-ref.inbox, effect.self-run, gate.tip-signed, scope=function, role=Verifies)
fn a_member_may_write_its_own_self_run_namespace() {
    let f = forge();
    let target = ObjectId::from_hex(b"abc1230000000000000000000000000000000000").expect("hex");
    let tip = result_commit(&f, "unit", target, vec![], Some(&f.guest), 300);
    let refname =
        namespace::self_result_ref(&MemberId::new("guest"), "unit", "abc123").expect("valid");
    expect_pass(&run(&f, &refname, Some(tip)), AdmissionKind::TipInvariant);
}

#[rstest]
#[case::own_segment_self_attested(false, "refs/meta/inbox/guest/issue-1", true)]
#[case::own_segment_admin(true, "refs/meta/inbox/admin/issue-1", true)]
#[case::foreign_segment_self_attested(false, "refs/meta/inbox/admin/issue-1", false)]
#[case::foreign_segment_even_for_admins(true, "refs/meta/inbox/guest/issue-1", false)]
#[case::unscoped_legacy_shape_owns_nothing(true, "refs/meta/inbox/issue-1", false)]
// @relation(meta-ref.inbox, model.member-provenance, gate.tip-signed, scope=function, role=Verifies)
fn inbox_segments_are_owner_only_for_both_provenances(
    #[case] as_admin: bool,
    #[case] refname: &str,
    #[case] admitted: bool,
) {
    let f = forge();
    let key = if as_admin { &f.admin } else { &f.guest };
    let new = commit(&f, vec![], Some(key), 300);
    let verdict = run(&f, &name(refname), Some(new));
    if admitted {
        expect_pass(&verdict, AdmissionKind::TipInvariant);
    } else {
        expect_fail(&verdict, Requirement::TipSigned);
    }
}

#[rstest]
// @relation(effect.admin-only, gate.tip-signed, scope=function, role=Verifies)
fn admin_registered_members_may_write_the_effects_namespace() {
    let f = forge();
    let effect = Effect {
        name: "unit".into(),
        trigger: "rev(refs/heads/main)".into(),
        toolchains: vec![],
        run: "true".into(),
    };
    let tree = facet_git_tree::serialize_into(&effect, &f.objects).expect("ser");
    let tip = tree_commit(&f, tree, vec![], Some(&f.admin), 300);
    expect_pass(
        &run(&f, &name("refs/meta/effects/unit"), Some(tip)),
        AdmissionKind::TipInvariant,
    );
}

// ---------------------------------------------------------------------
// Adoption and divergence: consequences of judging only the tip.
// ---------------------------------------------------------------------

#[rstest]
// @relation(gate.adoption-merge, scope=function, role=Verifies)
fn adoption_is_a_merge_that_keeps_the_contributor_commit_in_ancestry() {
    let f = forge();
    let genesis = commit(&f, vec![], Some(&f.admin), 300);
    let refname = name(&format!("refs/meta/comments/{genesis}"));
    f.refs.set(refname.as_ref(), genesis);
    // The contributor's own signed commit, not authorized for this ref.
    let contributed = commit(&f, vec![genesis], Some(&f.guest), 310);
    // The authorized member merges it: the merge tip satisfies the
    // invariant; the contributor's signature survives in ancestry.
    let merge = commit(&f, vec![genesis, contributed], Some(&f.admin), 320);
    expect_pass(&run(&f, &refname, Some(merge)), AdmissionKind::TipInvariant);
}

#[rstest]
// @relation(gate.adoption-no-fast-forward, scope=function, role=Verifies)
fn fast_forwarding_to_a_contributor_commit_is_not_adoption() {
    let f = forge();
    let genesis = commit(&f, vec![], Some(&f.admin), 300);
    let refname = name(&format!("refs/meta/comments/{genesis}"));
    f.refs.set(refname.as_ref(), genesis);
    let contributed = commit(&f, vec![genesis], Some(&f.guest), 310);
    // Descends fine — but the tip signature is the contributor's, and the
    // contributor is not authorized for this refname.
    expect_fail(
        &run(&f, &refname, Some(contributed)),
        Requirement::TipSigned,
    );
}

#[rstest]
// @relation(gate.same-actor-divergence, scope=function, role=Verifies)
fn a_members_own_divergent_heads_merge_cleanly() {
    let f = forge();
    let genesis = commit(&f, vec![], Some(&f.admin), 300);
    let refname = issue_ref(genesis);
    f.refs.set(refname.as_ref(), genesis);
    // Two of the member's own machines raced the single-writer ref.
    let a = commit(&f, vec![genesis], Some(&f.admin), 310);
    let b = commit(&f, vec![genesis], Some(&f.admin), 311);
    let merge = commit(&f, vec![a, b], Some(&f.admin), 320);
    expect_pass(&run(&f, &refname, Some(merge)), AdmissionKind::TipInvariant);
}

// ---------------------------------------------------------------------
// The verification epoch.
// ---------------------------------------------------------------------

#[rstest]
// @relation(gate.epoch, scope=function, role=Verifies)
fn before_any_epoch_meta_history_is_archival() {
    let refs = MemRefStore::default();
    let objects = ObjectStore::default();
    let f = Forge {
        refs,
        objects,
        admin: Keypair::from_seed(ADMIN_SEED),
        guest: Keypair::from_seed(SELF_ATTESTED_SEED),
    };
    // No config, no members: an unsigned meta write passes as pre-epoch.
    let new = commit(&f, vec![], None, 100);
    expect_pass(
        &run(&f, &issue_ref(new), Some(new)),
        AdmissionKind::PreEpoch,
    );
}

#[rstest]
// @relation(gate.epoch, scope=function, role=Verifies)
fn the_epoch_setting_commit_is_itself_the_first_gated_tip() {
    let refs = MemRefStore::default();
    let objects = ObjectStore::default();
    let admin = Keypair::from_seed(ADMIN_SEED);
    enroll_member(
        &refs,
        &objects,
        "admin",
        &admin,
        Provenance::AdminRegistered,
        100,
    );
    let f = Forge {
        refs,
        objects,
        admin,
        guest: Keypair::from_seed(SELF_ATTESTED_SEED),
    };

    let tree = facet_git_tree::serialize_into(&Config { epoch: Some(200) }, &f.objects)
        .expect("config serializes");
    let make = |key: Option<&Keypair>| {
        write_commit(
            &f.objects,
            &CommitSpec {
                tree,
                parents: vec![],
                message: "enable verification".into(),
                seconds: 200,
            },
            key,
        )
    };

    // Unsigned epoch-setting is refused: the circularity resolves by
    // gating the very commit that turns gating on.
    expect_fail(
        &run(&f, &name(namespace::CONFIG_REF), Some(make(None))),
        Requirement::TipSigned,
    );
    // Signed by an enrolled member, it passes under the tip invariant.
    expect_pass(
        &run(&f, &name(namespace::CONFIG_REF), Some(make(Some(&f.admin)))),
        AdmissionKind::TipInvariant,
    );
}

// ---------------------------------------------------------------------
// Bootstrap: fail-closed empty-member-list handling.
// ---------------------------------------------------------------------

/// A store with verification in force but no members at all.
fn bare_forge_with_epoch() -> Forge {
    let refs = MemRefStore::default();
    let objects = ObjectStore::default();
    let config_ref: FullName = namespace::CONFIG_REF.try_into().expect("valid");
    write_meta_entity(
        &refs,
        &objects,
        config_ref,
        &Config { epoch: Some(50) },
        None,
        50,
    );
    Forge {
        refs,
        objects,
        admin: Keypair::from_seed(ADMIN_SEED),
        guest: Keypair::from_seed(SELF_ATTESTED_SEED),
    }
}

fn enrollment_proposal(f: &Forge, id: &str, enrolled: &Keypair, signer: &Keypair) -> ObjectId {
    let member = Member::new(id, enrolled.public_openssh(), Provenance::AdminRegistered);
    let tree = facet_git_tree::serialize_into(&member, &f.objects).expect("member serializes");
    write_commit(
        &f.objects,
        &CommitSpec {
            tree,
            parents: vec![],
            message: format!("enroll {id}"),
            seconds: 100,
        },
        Some(signer),
    )
}

#[rstest]
// @relation(gate.bootstrap, scope=function, role=Verifies)
fn first_enrollment_is_self_admitting() {
    let f = bare_forge_with_epoch();
    let new = enrollment_proposal(&f, "first", &f.admin, &f.admin);
    expect_pass(
        &run(&f, &name("refs/meta/member/first"), Some(new)),
        AdmissionKind::Bootstrap,
    );
}

#[rstest]
// @relation(gate.bootstrap, gate.identity-binding, scope=function, role=Verifies)
fn a_bootstrap_enrollment_naming_the_wrong_ref_is_refused() {
    // Even the self-admitting bootstrap write is bound by the member's own
    // id field, not a trailer: an enrollment whose id is `first` cannot
    // land on `refs/meta/member/other`.
    let f = bare_forge_with_epoch();
    let new = enrollment_proposal(&f, "first", &f.admin, &f.admin);
    expect_fail(
        &run(&f, &name("refs/meta/member/other"), Some(new)),
        Requirement::IdentityBinding,
    );
}

#[rstest]
// @relation(gate.bootstrap, scope=function, role=Verifies)
fn bootstrap_enrollment_must_be_signed_by_the_key_it_enrolls() {
    let f = bare_forge_with_epoch();
    let other = Keypair::from_seed(OUTSIDER_SEED);
    let new = enrollment_proposal(&f, "first", &f.admin, &other);
    expect_fail(
        &run(&f, &name("refs/meta/member/first"), Some(new)),
        Requirement::TipSigned,
    );
}

#[rstest]
// @relation(gate.bootstrap, scope=function, role=Verifies)
fn bootstrap_admits_only_enrollments() {
    let f = bare_forge_with_epoch();
    let new = commit(&f, vec![], Some(&f.admin), 100);
    expect_fail(&run(&f, &issue_ref(new), Some(new)), Requirement::TipSigned);
}

#[rstest]
// @relation(gate.bootstrap, model.member-revocation, scope=function, role=Verifies)
fn revoking_every_key_does_not_reopen_the_bootstrap_window() {
    let f = forge();
    for id in ["admin", "guest"] {
        let key = if id == "admin" { &f.admin } else { &f.guest };
        let provenance = if id == "admin" {
            Provenance::AdminRegistered
        } else {
            Provenance::SelfAttested
        };
        let mut member = Member::new(id, key.public_openssh(), provenance);
        member.revoke();
        write_member(&f.refs, &f.objects, id, &member, Some(&f.admin), 400);
    }
    // A would-be new member self-enrolling: the member set is non-empty
    // (though fully revoked), so the self-admitting window stays shut.
    let newcomer = Keypair::from_seed(OUTSIDER_SEED);
    let new = enrollment_proposal(&f, "newcomer", &newcomer, &newcomer);
    expect_fail(
        &run(&f, &name("refs/meta/member/newcomer"), Some(new)),
        Requirement::TipSigned,
    );
    // And the fully-revoked members cannot write anything either.
    let attempt = commit(&f, vec![], Some(&f.admin), 500);
    expect_fail(
        &run(&f, &issue_ref(attempt), Some(attempt)),
        Requirement::TipSigned,
    );
}

// ---------------------------------------------------------------------
// CAS binding, call sites, and offline reproducibility.
// ---------------------------------------------------------------------

#[rstest]
// @relation(gate.atomic-cas, scope=function, role=Verifies)
fn the_admission_carries_the_cas_precondition_from_the_same_read() {
    let f = forge();

    // Creation: the ref must still be absent at write time.
    let created = commit(&f, vec![], Some(&f.admin), 300);
    let refname = issue_ref(created);
    let Verdict::Pass(admission) = run(&f, &refname, Some(created)) else {
        panic!("expected a pass");
    };
    assert_eq!(admission.cas, Expected::MustNotExist);

    // Update: the precondition is exactly the old tip the FF check used.
    f.refs.set(refname.as_ref(), created);
    let advanced = commit(&f, vec![created], Some(&f.admin), 320);
    let Verdict::Pass(admission) = run(&f, &refname, Some(advanced)) else {
        panic!("expected a pass");
    };
    assert_eq!(admission.cas, Expected::MustExistAndMatch(created));
}

/// Every scenario the verdict table distinguishes, evaluated the way each
/// of the three call sites would evaluate it, in one parameterized test.
#[rstest]
#[case::authorized_pass(true, true)]
#[case::unsigned_fail(false, true)]
#[case::unauthorized_namespace_guest(true, false)]
// @relation(gate.call-sites, gate.mandatory-hosted, gate.advisory-local, scope=function, role=Verifies)
fn all_three_call_sites_return_identical_verdicts(#[case] signed: bool, #[case] as_admin: bool) {
    let f = forge();
    let key = if as_admin { &f.admin } else { &f.guest };
    let new = commit(&f, vec![], signed.then_some(key), 300);
    let update = Update {
        name: issue_ref(new),
        new: Some(new),
    };

    // Call site 1: hosted CAS time (mandatory — the caller aborts on Fail).
    let hosted = verify(&f.refs, &f.objects, &update).expect("verdict");
    // Call site 2: the local UI verdict (advisory — annotates the write).
    let local = verify(&f.refs, &f.objects, &update).expect("verdict");
    // Call site 3: push pre-flight, against a fetched copy of the refs.
    let fetched = f.refs.fetched_copy();
    let preflight = verify(&fetched, &f.objects, &update).expect("verdict");

    assert_eq!(hosted, local, "hosted vs local");
    assert_eq!(hosted, preflight, "hosted vs pre-flight");
}

#[rstest]
// @relation(gate.signature-artifact, gate.policy-as-state, scope=function, role=Verifies)
fn verdicts_reproduce_offline_from_repository_state_alone() {
    let build = || {
        let f = forge();
        let new = commit(&f, vec![], Some(&f.admin), 300);
        (f, new)
    };
    let (origin, new_at_origin) = build();
    let (clone, new_at_clone) = build();
    assert_eq!(new_at_origin, new_at_clone, "deterministic fixtures");

    let update = Update {
        name: issue_ref(new_at_origin),
        new: Some(new_at_origin),
    };
    let at_origin = verify(&origin.refs, &origin.objects, &update).expect("verdict");
    let at_clone = verify(&clone.refs, &clone.objects, &update).expect("verdict");
    assert_eq!(at_origin, at_clone);
}

#[rstest]
// @relation(gate.verdict-reason, gate.advisory-local, scope=function, role=Verifies)
fn refusals_render_an_actionable_reason_with_the_inbox_alternative() {
    let f = forge();
    let new = commit(&f, vec![], Some(&f.guest), 300);
    let refname = issue_ref(new);
    let Verdict::Fail(refusal) = run(&f, &refname, Some(new)) else {
        panic!("self-attested member on a canonical ref must be refused");
    };
    let rendered = refusal.to_string();
    assert!(
        rendered.contains("gate.tip-signed"),
        "names the rule: {rendered}"
    );
    assert!(
        rendered.contains("refs/meta/issues/"),
        "names the subject ref: {rendered}"
    );
    assert!(
        rendered.contains("refs/meta/inbox"),
        "surfaces the inbox alternative at verdict time: {rendered}"
    );
}

#[rstest]
// @relation(gate.epoch, scope=function, role=Verifies)
fn config_round_trips_with_and_without_an_epoch() {
    for config in [Config { epoch: None }, Config { epoch: Some(42) }] {
        let (root, store) = facet_git_tree::serialize(&config).expect("serialize");
        let back: Config = facet_git_tree::deserialize(&root, &store).expect("deserialize");
        assert_eq!(back, config);
    }
}

#[rstest]
fn fixture_stores_read_back_what_they_seed() {
    let f = forge();
    let member_ref = name("refs/meta/member/admin");
    assert!(f.refs.get(member_ref.as_ref()).expect("readable").is_some());
}
