//! Counterexample tests for `verify/ledger.adoc` — the formal-stocktake
//! verdict ledger's landing strip in code.
//!
//! Convention (see `verify/README.adoc`): a ledger row found FALSIFIED or
//! DIVERGED with a concrete, transaction-shaped counterexample lands here
//! first, expressed in this crate's own [`Facts`] vocabulary. While the
//! gap is open, the test is a *gap-pinning* test: it asserts the current
//! (wrong-per-the-docs) behavior, so the suite stays green and the gap
//! stays visible. When the missing denial rule is added — one rule at a
//! time, red test first, per this crate's own discipline — the pinned
//! assertion is swapped for the inverted one kept alongside it, and the
//! ledger row's verdict is updated.
//!
//! Where the counterexample is transaction-shaped like this one, the
//! same landing happens in two more places, all three updated together:
//! `tests/prop.rs`'s matching property loses its exemption branch (it
//! currently proves the exemption load-bearing by failing without it),
//! and `crates/verify/ents-verify/src/search.rs`'s matching
//! [`stateright::Property::always`] — which today has a discovery — is
//! expected to have none once the rule lands.

use ents_gate_rules::{Facts, Role, gate};

/// Cross-ref replay through the missing refname-binding rule.
///
/// Ledger row: `docs/abstractions.adoc` §4 / `docs/spec/meta-ref.adoc`
/// `meta-ref.identity-binding` — "the refname is a total function of
/// signed content, recomputed at verification". Verdict: DIVERGED (the
/// doc claims it; no rule checks it; no gap marker declares the
/// omission). Missing rule: `binding_violation`. Alloy witness:
/// `verify/alloy/gate_rules.als`, check `binding_refname_recomputed`.
///
/// The transaction: an admin-signed, parentless commit whose signed
/// content is a *comment* (anchor + context blobs present and resolving),
/// replayed as the creation of `refs/meta/effects/x`. Every current rule
/// is satisfied: `genesis` (parentless), `unsigned` (member-signed),
/// `effect_admin` (admin-signed), `ff` (vacuous — creation), the root and
/// anchor rules likewise. Nothing recomputes the refname from the signed
/// content, so the comment is admitted as an effect definition.
///
/// This test PINS the open gap: it asserts the replay is admitted today.
/// Fixing it is out of scope for the stocktake scaffolding. When
/// `binding_violation` lands, this assertion flips — swap it for the
/// commented one below.
#[test]
fn cross_ref_replay_of_comment_as_effect_is_admitted_today() {
    let mut facts = Facts {
        member: vec![("key:admin".into(), Role::Admin)],
        ..Facts::default()
    };
    // A comment-shaped genesis: parentless, admin-signed, embedding its
    // anchored blob and context blob — but pushed as the creation of an
    // effects ref, a namespace its signed content does not derive.
    facts.ref_update = vec![("refs/meta/effects/x".into(), None, "g2".into())];
    facts.signed_by = vec![("g2".into(), "key:admin".into())];
    facts.anchor = vec![("g2".into(), "blob:a".into())];
    facts.context = vec![("g2".into(), "blob:ctx".into())];
    facts.object_exists = vec![("blob:a".into(),), ("blob:ctx".into(),)];

    let verdicts = gate(facts);

    // Pinned current behavior: admitted. This is the gap, kept green on
    // purpose so CI never normalizes ignoring it.
    assert!(
        verdicts.is_empty(),
        "the binding gap appears to have closed: a rule now denies the \
         cross-ref replay ({verdicts:?}) — flip this test to the inverted \
         assertion below and update verify/ledger.adoc"
    );

    // Ready to swap in when `binding_violation` exists:
    // assert!(
    //     verdicts.iter().any(|v| v.starts_with("binding:")),
    //     "binding_violation must deny the cross-ref replay: {verdicts:?}"
    // );
}
