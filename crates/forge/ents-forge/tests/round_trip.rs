//! Property-based round-trip test for `meta-ref.typed-tree`: struct → tree
//! → struct must be identity over an unenumerable input space (arbitrary
//! strings, arbitrary-length collections) — the shape
//! `git-ents-engineering` calls out for `proptest` rather than a fixed
//! `rstest` table. [`Issue`] is exercised directly, as the richest entity
//! this crate owns (ported from `ents-model`'s own `tests/round_trip.rs`
//! when [`Issue`] moved here; [`ents_model::Member`] stays covered by that
//! crate's own analogous test).

#![allow(clippy::expect_used, reason = "integration test")]

use ents_forge::Issue;
use ents_model::MemberId;
use facet_git_tree::{deserialize, serialize};
use proptest::prelude::*;

fn member_id() -> impl Strategy<Value = MemberId> {
    any::<String>().prop_map(MemberId::new)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

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
