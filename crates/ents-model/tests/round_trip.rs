//! Property-based round-trip tests for `meta-ref.typed-tree`: struct → tree
//! → struct must be identity over an unenumerable input space (arbitrary
//! strings, arbitrary-length collections) — the shape of test
//! `git-ents-engineering` calls out for `proptest` rather than a fixed
//! `rstest` table. [`Issue`] and [`Member`] are exercised directly, as the
//! richest and the enum-heaviest of this crate's entities; every other
//! entity's round trip is covered by the fixed-case table in its own
//! module (`model.comment`, `model.effect-definition`, `model.toolchain`,
//! `model.redaction`, `model.account`, `model.result-taxonomy`).

#![allow(clippy::expect_used, reason = "integration test")]

use ents_model::{Issue, Member, MemberId, MemberState, Provenance};
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

fn member_id() -> impl Strategy<Value = MemberId> {
    any::<String>().prop_map(MemberId::new)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // @relation(meta-ref.typed-tree, scope=function, role=Verifies)
    #[test]
    fn member_round_trips_for_any_key_state_and_provenance(
        key in any::<String>(),
        state in member_state(),
        provenance in provenance(),
    ) {
        let member = Member { key, state, provenance };
        let (id, store) = serialize(&member).expect("serialize");
        let back: Member = deserialize(&id, &store).expect("deserialize");
        prop_assert_eq!(back, member);
    }

    // @relation(meta-ref.typed-tree, model.issue, scope=function, role=Verifies)
    #[test]
    fn issue_round_trips_for_any_fields_and_collection_lengths(
        title in any::<String>(),
        body in any::<String>(),
        state in any::<String>(),
        assignees in prop::collection::vec(member_id(), 0..8),
        labels in prop::collection::vec(any::<String>(), 0..8),
    ) {
        let issue = Issue { title, body, state, assignees, labels };
        let (id, store) = serialize(&issue).expect("serialize");
        let back: Issue = deserialize(&id, &store).expect("deserialize");
        prop_assert_eq!(back, issue);
    }
}
