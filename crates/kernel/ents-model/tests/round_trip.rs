//! Property-based round-trip tests for `meta-ref.typed-tree`: struct → tree
//! → struct must be identity over an unenumerable input space (arbitrary
//! strings, arbitrary-length collections) — the shape of test
//! `git-ents-engineering` calls out for `proptest` rather than a fixed
//! `rstest` table. [`Member`] is exercised directly, as the
//! enum-heaviest of this crate's entities; every other entity's round trip
//! is covered by the fixed-case table in its own module (`model.comment`,
//! `model.effect-definition`, `model.toolchain`, `model.redaction`,
//! `model.account`, `model.result-taxonomy`). [`ents_model::Issue`] moved to
//! `ents-forge` along with its own analogous property test
//! (`that crate's own tests/round_trip.rs`).

#![allow(clippy::expect_used, reason = "integration test")]

use ents_model::{Member, MemberState, Provenance};
use facet_git_tree::{deserialize, serialize};
use proptest::prelude::*;

fn member_state() -> impl Strategy<Value = MemberState> {
    prop_oneof![Just(MemberState::Active), Just(MemberState::Revoked)]
}

fn provenance() -> impl Strategy<Value = Provenance> {
    prop_oneof![
        Just(Provenance::AdminRegistered),
        Just(Provenance::SelfAttested)
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // @relation(meta-ref.typed-tree, scope=function, role=Verifies)
    #[test]
    fn member_round_trips_for_any_key_state_and_provenance(
        id in any::<String>(),
        key in any::<String>(),
        state in member_state(),
        provenance in provenance(),
    ) {
        let member = Member { id: ents_model::MemberId::new(id), key, state, provenance };
        let (id, store) = serialize(&member).expect("serialize");
        let back: Member = deserialize(&id, &store).expect("deserialize");
        prop_assert_eq!(back, member);
    }
}
