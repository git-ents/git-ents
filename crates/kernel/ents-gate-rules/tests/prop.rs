//! Property-based floor for the ledger's invariant claims
//! (`verify/ledger.adoc`): `proptest` samples `Facts` over a small
//! bounded domain — deliberately duplicated here from
//! `crates/verify/ents-verify`'s vocabulary rather than depending on
//! that crate, since the sink-layer rule in
//! `crates/cli/git-ents/tests/layering.rs` is one-way: nothing may
//! depend on `ents-verify`.
//!
//! Two properties below have no exemption — the rules that cover them
//! (`unsigned_violation`, `effect_admin_violation`) are believed sound.
//! The third carries the one *known* exemption in this crate: the
//! refname-binding gap (`verify/ledger.adoc`, DIVERGED row;
//! `docs/abstractions.adoc` §2). A naive "admitted implies bound
//! correctly" property would fail on every run while that gap is open;
//! the exemption proves it is load-bearing, not dead code, and per
//! `tests/ledger.rs`'s convention, gets deleted in the same commit that
//! adds `binding_violation`.

use ents_gate_rules::{Facts, Role, gate};
use proptest::prelude::*;

/// The signed content's own declared kind — the same shadow annotation
/// `ents-verify`'s search model and `verify/alloy/gate_rules.als`'s
/// `kind` field use, kept outside `Facts` because its absence from the
/// crate's real vocabulary *is* the gap under test.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Comment,
    Issue,
    Effect,
}

const ADMIN_KEY: &str = "key:admin";
const MEMBER_KEY: &str = "key:m1";

const ISSUE_REF: &str = "refs/meta/issues/g";
const COMMENT_REF: &str = "refs/meta/comments/g2";
const EFFECT_REF: &str = "refs/meta/effects/x";

fn refs() -> impl Strategy<Value = &'static str> {
    prop_oneof![Just(ISSUE_REF), Just(COMMENT_REF), Just(EFFECT_REF)]
}

fn signers() -> impl Strategy<Value = Option<&'static str>> {
    prop_oneof![Just(Some(ADMIN_KEY)), Just(Some(MEMBER_KEY)), Just(None)]
}

fn kinds() -> impl Strategy<Value = Kind> {
    prop_oneof![Just(Kind::Comment), Just(Kind::Issue), Just(Kind::Effect)]
}

/// Build a genesis transaction — the only shape the binding gap
/// concerns, since binding is a claim about a fresh entity's placement
/// — from sampled atoms, alongside whether the doc's binding invariant
/// actually holds for this sample.
fn build(ref_name: &str, signer: Option<&str>, kind: Kind) -> (Facts, bool) {
    let mut facts = Facts {
        member: vec![(ADMIN_KEY.to_string(), Role::Admin), (MEMBER_KEY.to_string(), Role::Member)],
        ..Facts::default()
    };
    facts.ref_update = vec![(ref_name.to_string(), None, "g2".to_string())];
    if let Some(key) = signer {
        facts.signed_by = vec![("g2".to_string(), key.to_string())];
    }

    let binding_holds = match kind {
        Kind::Effect => ref_name.starts_with("refs/meta/effects/"),
        Kind::Comment => ref_name.starts_with("refs/meta/comments/"),
        Kind::Issue => ref_name.starts_with("refs/meta/issues/"),
    };
    (facts, binding_holds)
}

proptest! {
    /// Ledger floor (abstractions.adoc §5 tip invariant, admission
    /// half): an admitted genesis is signed by an enrolled member. No
    /// exemption — `unsigned_violation` covers this today.
    #[test]
    fn admitted_genesis_is_signed(r in refs(), signer in signers(), kind in kinds()) {
        let (facts, _binding_holds) = build(r, signer, kind);
        if gate(facts).is_empty() {
            prop_assert!(signer.is_some());
        }
    }

    /// Ledger floor (abstractions.adoc §6 / effect.admin-only): an
    /// admitted write to `refs/meta/effects/*` is admin-signed. No
    /// exemption — `effect_admin_violation` covers this today.
    #[test]
    fn admitted_effects_write_is_admin_signed(signer in signers(), kind in kinds()) {
        let (facts, _binding_holds) = build(EFFECT_REF, signer, kind);
        if gate(facts).is_empty() {
            prop_assert_eq!(signer, Some(ADMIN_KEY));
        }
    }

    /// Ledger row (DIVERGED, `docs/abstractions.adoc` §2 /
    /// `meta-ref.identity-binding`): an admitted genesis's refname
    /// namespace should match its signed content's declared kind.
    /// EXEMPTED while the gap is open — delete the early return (and
    /// this comment) in the same commit that adds `binding_violation`,
    /// per `tests/ledger.rs`'s gap-pinning convention.
    #[test]
    fn admitted_genesis_binds_its_namespace(r in refs(), signer in signers(), kind in kinds()) {
        let (facts, binding_holds) = build(r, signer, kind);
        if gate(facts.clone()).is_empty() && !binding_holds {
            // KNOWN GAP (verify/ledger.adoc: DIVERGED). Keeping this
            // branch, rather than deleting the property outright, is
            // what proves the exemption is load-bearing: comment it out
            // locally and this property fails immediately.
            return Ok(());
        }
        if gate(facts).is_empty() {
            prop_assert!(binding_holds);
        }
    }
}
