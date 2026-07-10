//! Integration tests for the gate: the verdict table (rstest — the spec
//! enumerates the cases), the epoch and bootstrap windows, and the
//! one-parameterized-test proof that all three call sites see identical
//! verdicts.

#![expect(
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    reason = "integration test: fixtures panic on setup failure"
)]

use ents_gate::{AdmissionKind, Config, Requirement, Update, Verdict, verify};
use ents_model::trailer::Trailers;
use ents_model::{Member, MemberId, Provenance, namespace};
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

/// A proposal commit: empty tree, explicit parents/trailer/key/time.
fn proposal(
    forge: &Forge,
    parents: Vec<ObjectId>,
    ents_ref: Option<&str>,
    key: Option<&Keypair>,
    seconds: i64,
) -> ObjectId {
    let tree = empty_tree(&forge.objects);
    let message = ents_ref.map_or_else(
        || "mutate\n\nno trailer here\n".to_owned(),
        |r| {
            let trailers = Trailers {
                ents_ref: Some(name(r)),
                schema_version: None,
            };
            format!("mutate\n\n{}", trailers.render())
        },
    );
    write_commit(
        &forge.objects,
        &CommitSpec {
            tree,
            parents,
            message,
            seconds,
        },
        key,
    )
}

fn run(forge: &Forge, refname: &str, new: Option<ObjectId>) -> Verdict {
    verify(
        &forge.refs,
        &forge.objects,
        &Update {
            name: name(refname),
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
// The verdict table: member × trailer × FF × namespace cases.
// ---------------------------------------------------------------------

#[rstest]
// @relation(gate.tip-signed, gate.verdict-reason, scope=function, role=Verifies)
fn authorized_signed_mutation_passes_the_tip_invariant() {
    let f = forge();
    let new = proposal(&f, vec![], Some("refs/meta/issues/1"), Some(&f.admin), 300);
    expect_pass(
        &run(&f, "refs/meta/issues/1", Some(new)),
        AdmissionKind::TipInvariant,
    );
}

#[rstest]
// @relation(gate.tip-signed, scope=function, role=Verifies)
fn unsigned_tip_is_refused() {
    let f = forge();
    let new = proposal(&f, vec![], Some("refs/meta/issues/1"), None, 300);
    expect_fail(
        &run(&f, "refs/meta/issues/1", Some(new)),
        Requirement::TipSigned,
    );
}

#[rstest]
// @relation(gate.tip-signed, scope=function, role=Verifies)
fn non_member_signature_is_refused() {
    let f = forge();
    let outsider = Keypair::from_seed(OUTSIDER_SEED);
    let new = proposal(&f, vec![], Some("refs/meta/issues/1"), Some(&outsider), 300);
    expect_fail(
        &run(&f, "refs/meta/issues/1", Some(new)),
        Requirement::TipSigned,
    );
}

#[rstest]
// @relation(gate.tip-signed, model.member-revocation, scope=function, role=Verifies)
fn signature_made_after_revocation_is_refused() {
    let f = forge();
    let mut revoked = Member::new(f.admin.public_openssh(), Provenance::AdminRegistered);
    revoked.revoke();
    write_member(&f.refs, &f.objects, "admin", &revoked, Some(&f.admin), 400);

    let new = proposal(&f, vec![], Some("refs/meta/issues/1"), Some(&f.admin), 500);
    let verdict = run(&f, "refs/meta/issues/1", Some(new));
    expect_fail(&verdict, Requirement::TipSigned);
    let Verdict::Fail(refusal) = &verdict else {
        unreachable!()
    };
    assert!(refusal.detail.contains("revoked"), "detail: {refusal}");
}

#[rstest]
// @relation(model.member-revocation, gate.tip-signed, scope=function, role=Verifies)
fn signature_made_before_revocation_stays_verifiable() {
    // The boundary is found by walking the member ref's history and
    // deserializing the tree in force at the signature's timestamp —
    // there is no validity-window field, by design.
    let f = forge();
    let new = proposal(&f, vec![], Some("refs/meta/issues/1"), Some(&f.admin), 300);

    let mut revoked = Member::new(f.admin.public_openssh(), Provenance::AdminRegistered);
    revoked.revoke();
    write_member(&f.refs, &f.objects, "admin", &revoked, Some(&f.admin), 400);

    // The gate runs *after* the revocation, on a commit signed before it.
    expect_pass(
        &run(&f, "refs/meta/issues/1", Some(new)),
        AdmissionKind::TipInvariant,
    );
}

#[rstest]
#[case::wrong_ref(Some("refs/meta/issues/2"))]
#[case::missing_trailer(None)]
// @relation(gate.refname-binding, scope=function, role=Verifies)
fn refname_binding_mismatch_is_refused(#[case] trailer: Option<&str>) {
    let f = forge();
    let new = proposal(&f, vec![], trailer, Some(&f.admin), 300);
    expect_fail(
        &run(&f, "refs/meta/issues/1", Some(new)),
        Requirement::RefnameBinding,
    );
}

#[rstest]
// @relation(gate.fast-forward, scope=function, role=Verifies)
fn non_fast_forward_is_refused() {
    let f = forge();
    let refname = "refs/meta/issues/1";
    let tip = write_meta_entity(
        &f.refs,
        &f.objects,
        name(refname),
        &ents_model::Status::Pass,
        Some(&f.admin),
        300,
    );
    // A sibling that does not descend from `tip`.
    let sibling = proposal(&f, vec![], Some(refname), Some(&f.admin), 310);
    assert_ne!(sibling, tip);
    expect_fail(&run(&f, refname, Some(sibling)), Requirement::FastForward);
}

#[rstest]
// @relation(gate.fast-forward, scope=function, role=Verifies)
fn meta_ref_deletion_is_refused() {
    let f = forge();
    write_meta_entity(
        &f.refs,
        &f.objects,
        name("refs/meta/issues/1"),
        &ents_model::Status::Pass,
        Some(&f.admin),
        300,
    );
    expect_fail(
        &run(&f, "refs/meta/issues/1", None),
        Requirement::FastForward,
    );
}

#[rstest]
// @relation(gate.principled-split, scope=function, role=Verifies)
fn code_refs_are_not_subject_to_the_tip_invariant() {
    let f = forge();
    // Unsigned, trailerless, non-FF — none of it matters outside refs/meta/*.
    let new = proposal(&f, vec![], None, None, 300);
    expect_pass(
        &run(&f, "refs/heads/main", Some(new)),
        AdmissionKind::CodeRef,
    );
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
    let f = forge();
    let new = proposal(&f, vec![], Some(refname), Some(&f.guest), 300);
    let verdict = run(&f, refname, Some(new));
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
    let refname = "refs/meta/self/guest/unit/abc123";
    let new = proposal(&f, vec![], Some(refname), Some(&f.guest), 300);
    expect_pass(&run(&f, refname, Some(new)), AdmissionKind::TipInvariant);
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
    let new = proposal(&f, vec![], Some(refname), Some(key), 300);
    let verdict = run(&f, refname, Some(new));
    if admitted {
        expect_pass(&verdict, AdmissionKind::TipInvariant);
    } else {
        expect_fail(&verdict, Requirement::TipSigned);
    }
}

#[rstest]
#[case::effects("refs/meta/effects/unit")]
#[case::results("refs/meta/results/unit/abc")]
// @relation(effect.admin-only, gate.tip-signed, scope=function, role=Verifies)
fn admin_registered_members_may_write_canonical_namespaces(#[case] refname: &str) {
    let f = forge();
    let new = proposal(&f, vec![], Some(refname), Some(&f.admin), 300);
    expect_pass(&run(&f, refname, Some(new)), AdmissionKind::TipInvariant);
}

// ---------------------------------------------------------------------
// Adoption and divergence: consequences of judging only the tip.
// ---------------------------------------------------------------------

#[rstest]
// @relation(gate.adoption-merge, scope=function, role=Verifies)
fn adoption_is_a_merge_that_keeps_the_contributor_commit_in_ancestry() {
    let f = forge();
    let refname = "refs/meta/comments/c1";
    let tip = write_meta_entity(
        &f.refs,
        &f.objects,
        name(refname),
        &ents_model::Status::Pass,
        Some(&f.admin),
        300,
    );
    // The contributor's own signed commit, not authorized for this ref.
    let contributed = proposal(&f, vec![tip], Some(refname), Some(&f.guest), 310);
    // The authorized member merges it: the merge tip satisfies the
    // invariant; the contributor's signature survives in ancestry.
    let merge = proposal(
        &f,
        vec![tip, contributed],
        Some(refname),
        Some(&f.admin),
        320,
    );
    expect_pass(&run(&f, refname, Some(merge)), AdmissionKind::TipInvariant);
}

#[rstest]
// @relation(gate.adoption-no-fast-forward, scope=function, role=Verifies)
fn fast_forwarding_to_a_contributor_commit_is_not_adoption() {
    let f = forge();
    let refname = "refs/meta/comments/c1";
    let tip = write_meta_entity(
        &f.refs,
        &f.objects,
        name(refname),
        &ents_model::Status::Pass,
        Some(&f.admin),
        300,
    );
    let contributed = proposal(&f, vec![tip], Some(refname), Some(&f.guest), 310);
    // Descends fine — but the tip signature is the contributor's, and
    // the contributor is not authorized for this refname.
    expect_fail(&run(&f, refname, Some(contributed)), Requirement::TipSigned);
}

#[rstest]
// @relation(gate.same-actor-divergence, scope=function, role=Verifies)
fn a_members_own_divergent_heads_merge_cleanly() {
    let f = forge();
    let refname = "refs/meta/issues/1";
    let tip = write_meta_entity(
        &f.refs,
        &f.objects,
        name(refname),
        &ents_model::Status::Pass,
        Some(&f.admin),
        300,
    );
    // Two of the member's own machines raced the single-writer ref.
    let a = proposal(&f, vec![tip], Some(refname), Some(&f.admin), 310);
    let b = proposal(&f, vec![tip], Some(refname), Some(&f.admin), 311);
    // Either head alone is a non-fast-forward once the other landed;
    // the resolution is the member merging their own heads.
    let merge = proposal(&f, vec![a, b], Some(refname), Some(&f.admin), 320);
    expect_pass(&run(&f, refname, Some(merge)), AdmissionKind::TipInvariant);
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
    let new = proposal(&f, vec![], None, None, 100);
    expect_pass(
        &run(&f, "refs/meta/issues/1", Some(new)),
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
        let trailers = Trailers {
            ents_ref: Some(name(namespace::CONFIG_REF)),
            schema_version: None,
        };
        write_commit(
            &f.objects,
            &CommitSpec {
                tree,
                parents: vec![],
                message: format!("enable verification\n\n{}", trailers.render()),
                seconds: 200,
            },
            key,
        )
    };

    // Unsigned epoch-setting is refused: the circularity resolves by
    // gating the very commit that turns gating on.
    expect_fail(
        &run(&f, namespace::CONFIG_REF, Some(make(None))),
        Requirement::TipSigned,
    );
    // Signed by an enrolled member, it passes under the tip invariant.
    expect_pass(
        &run(&f, namespace::CONFIG_REF, Some(make(Some(&f.admin)))),
        AdmissionKind::TipInvariant,
    );
}

// ---------------------------------------------------------------------
// Bootstrap: fail-closed empty-member-list handling.
// ---------------------------------------------------------------------

/// A store with verification in force but no members at all — the shape
/// a hosted deployment initializes (`roots.bootstrap` owns hardening).
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
    let member = Member::new(enrolled.public_openssh(), Provenance::AdminRegistered);
    let tree = facet_git_tree::serialize_into(&member, &f.objects).expect("member serializes");
    let refname = namespace::member_ref(&MemberId::new(id)).expect("valid id");
    let trailers = Trailers {
        ents_ref: Some(refname),
        schema_version: None,
    };
    write_commit(
        &f.objects,
        &CommitSpec {
            tree,
            parents: vec![],
            message: format!("enroll {id}\n\n{}", trailers.render()),
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
        &run(&f, "refs/meta/member/first", Some(new)),
        AdmissionKind::Bootstrap,
    );
}

#[rstest]
// @relation(gate.bootstrap, scope=function, role=Verifies)
fn bootstrap_enrollment_must_be_signed_by_the_key_it_enrolls() {
    let f = bare_forge_with_epoch();
    let other = Keypair::from_seed(OUTSIDER_SEED);
    let new = enrollment_proposal(&f, "first", &f.admin, &other);
    expect_fail(
        &run(&f, "refs/meta/member/first", Some(new)),
        Requirement::TipSigned,
    );
}

#[rstest]
// @relation(gate.bootstrap, scope=function, role=Verifies)
fn bootstrap_admits_only_enrollments() {
    let f = bare_forge_with_epoch();
    let new = proposal(&f, vec![], Some("refs/meta/issues/1"), Some(&f.admin), 100);
    expect_fail(
        &run(&f, "refs/meta/issues/1", Some(new)),
        Requirement::TipSigned,
    );
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
        let mut member = Member::new(key.public_openssh(), provenance);
        member.revoke();
        write_member(&f.refs, &f.objects, id, &member, Some(&f.admin), 400);
    }
    // A would-be new member self-enrolling: the member set is non-empty
    // (though fully revoked), so the self-admitting window stays shut.
    let newcomer = Keypair::from_seed(OUTSIDER_SEED);
    let new = enrollment_proposal(&f, "newcomer", &newcomer, &newcomer);
    expect_fail(
        &run(&f, "refs/meta/member/newcomer", Some(new)),
        Requirement::TipSigned,
    );
    // And the fully-revoked members cannot write anything either.
    let attempt = proposal(&f, vec![], Some("refs/meta/issues/9"), Some(&f.admin), 500);
    expect_fail(
        &run(&f, "refs/meta/issues/9", Some(attempt)),
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
    let refname = "refs/meta/issues/1";

    // Creation: the ref must still be absent at write time.
    let created = proposal(&f, vec![], Some(refname), Some(&f.admin), 300);
    let Verdict::Pass(admission) = run(&f, refname, Some(created)) else {
        panic!("expected a pass");
    };
    assert_eq!(admission.cas, Expected::MustNotExist);

    // Update: the precondition is exactly the old tip the FF check used.
    let tip = write_meta_entity(
        &f.refs,
        &f.objects,
        name(refname),
        &ents_model::Status::Pass,
        Some(&f.admin),
        310,
    );
    let advanced = proposal(&f, vec![tip], Some(refname), Some(&f.admin), 320);
    let Verdict::Pass(admission) = run(&f, refname, Some(advanced)) else {
        panic!("expected a pass");
    };
    assert_eq!(admission.cas, Expected::MustExistAndMatch(tip));
}

/// Every scenario the verdict table distinguishes, evaluated the way
/// each of the three call sites would evaluate it — hosted CAS on the
/// live store, local UI verdict on the same store, and push pre-flight
/// on a fetched copy of the refs — in one parameterized test: the gate
/// is one function, and its verdict is identical at every site.
#[rstest]
#[case::authorized_pass("refs/meta/issues/1", true, true, 300)]
#[case::unsigned_fail("refs/meta/issues/1", false, true, 300)]
#[case::unauthorized_namespace("refs/meta/effects/unit", true, false, 300)]
#[case::self_run_pass("refs/meta/self/guest/unit/abc", true, false, 300)]
// @relation(gate.call-sites, gate.mandatory-hosted, gate.advisory-local, scope=function, role=Verifies)
fn all_three_call_sites_return_identical_verdicts(
    #[case] refname: &str,
    #[case] signed: bool,
    #[case] as_admin: bool,
    #[case] seconds: i64,
) {
    let f = forge();
    let key = if as_admin { &f.admin } else { &f.guest };
    let new = proposal(&f, vec![], Some(refname), signed.then_some(key), seconds);
    let update = Update {
        name: name(refname),
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
    // Build the identical repository twice from deterministic seeds —
    // the fixture analogue of verifying in an independent clone. The
    // verdict depends only on refs/meta/* state and object bytes, so
    // both "clones" agree, with no transport artifact consulted.
    let build = || {
        let f = forge();
        let new = proposal(&f, vec![], Some("refs/meta/issues/1"), Some(&f.admin), 300);
        (f, new)
    };
    let (origin, new_at_origin) = build();
    let (clone, new_at_clone) = build();
    assert_eq!(new_at_origin, new_at_clone, "deterministic fixtures");

    let update = Update {
        name: name("refs/meta/issues/1"),
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
    let new = proposal(&f, vec![], Some("refs/meta/issues/1"), Some(&f.guest), 300);
    let Verdict::Fail(refusal) = run(&f, "refs/meta/issues/1", Some(new)) else {
        panic!("self-attested member on a canonical ref must be refused");
    };
    let rendered = refusal.to_string();
    assert!(
        rendered.contains("gate.tip-signed"),
        "names the rule: {rendered}"
    );
    assert!(
        rendered.contains("refs/meta/issues/1"),
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
    // Guards the fixture itself: the enrolled member refs exist and the
    // ref store returns them through the production read trait.
    let f = forge();
    let member_ref = name("refs/meta/member/admin");
    assert!(f.refs.get(member_ref.as_ref()).expect("readable").is_some());
}
