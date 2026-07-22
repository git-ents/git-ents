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

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // @relation(meta-ref.typed-tree, scope=function, role=Verifies)
    #[test]
    fn agent_session_round_trips_for_any_fields_and_collection_lengths(
        member in any::<String>(),
        created in any::<i64>(),
        model in any::<String>(),
        toolchain_names in prop::collection::vec(any::<String>(), 0..4),
        base_ref in any::<String>(),
        plan in proptest::option::of(any::<String>()),
        thread in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..16), 0..4),
    ) {
        use ents_forge::agent::{AgentSession, Confirm, ReviewPolicy, SessionMeta, ToolchainPin};

        let toolchains: Vec<ToolchainPin> = toolchain_names
            .into_iter()
            .map(|name| ToolchainPin::new(name, gix_hash::ObjectId::null(gix_hash::Kind::Sha1)))
            .collect();
        let meta = SessionMeta::new(
            MemberId::new(member),
            created,
            model,
            toolchains,
            base_ref,
            ReviewPolicy::Manual,
            None,
        );
        // A confirm can only exist alongside a plan in practice (the command
        // layer refuses otherwise), but the typed tree itself places no such
        // constraint on what round-trips — exercised here as `plan`'s own
        // hash, present only when `plan` is.
        let confirm = plan
            .as_deref()
            .map(|text| Confirm::new(
                gix_object::compute_hash(gix_hash::Kind::Sha1, gix_object::Kind::Blob, text.as_bytes())
                    .expect("hashing cannot fail"),
                ReviewPolicy::Auto,
            ));
        let session = AgentSession { meta, plan, confirm, thread };
        let (id, store) = serialize(&session).expect("serialize");
        let back: AgentSession = deserialize(&id, &store).expect("deserialize");
        prop_assert_eq!(back, session);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // @relation(meta-ref.typed-tree, model.comment, scope=function, role=Verifies)
    #[test]
    fn comment_round_trips_for_any_fields(
        body in any::<String>(),
        state in any::<String>(),
        context in proptest::option::of(any::<String>()),
        parent in proptest::option::of(any::<String>()),
        anchored in any::<bool>(),
    ) {
        use ents_forge::comment::Comment;
        use facet_git_tree::{ObjectStore, RawTree};
        use gix_object::Write as _;

        let store = ObjectStore::default();
        let anchor = anchored.then(|| {
            let tree = gix_object::Tree { entries: vec![] };
            RawTree::new(store.write(&tree).expect("tree"))
        });
        let comment = Comment { body, state, anchor, context, parent };
        let root = facet_git_tree::serialize_into(&comment, &store).expect("serialize");
        let back: Comment = facet_git_tree::deserialize(&root, &store).expect("deserialize");
        prop_assert_eq!(back, comment);
    }
}
