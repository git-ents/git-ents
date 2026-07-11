//! Property tests for the schema-aware three-way merge (`sync.divergence-merge`).
//!
//! Strategy: **proptest** — the spec states an algebraic invariant (a merge
//! that respects the typed tree's schema, field by field) over an
//! unenumerable input space, so example rows cannot stand in for it. The
//! properties pinned here are the ones a schema-aware merge must uphold:
//! disjoint field edits combine (never conflict), the same field changed
//! two ways conflicts, and the merge is commutative in its two sides.

#![expect(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use ents_model::{Issue, MemberId};
use ents_sync::merge::{Merge, three_way};
use ents_testutil::ObjectStore;
use proptest::prelude::*;

/// A base issue with room to edit every field independently.
fn issue_strategy() -> impl Strategy<Value = Issue> {
    (
        "[a-z]{1,8}",
        "[a-z]{1,8}",
        prop::sample::select(vec!["open", "closed"]),
        prop::collection::vec("[a-z]{1,5}", 0..3),
        prop::collection::vec("[a-z]{1,5}", 0..3),
    )
        .prop_map(|(title, body, state, assignees, labels)| Issue {
            title,
            body,
            state: state.to_string(),
            assignees: assignees.into_iter().map(MemberId::new).collect(),
            labels,
        })
}

/// Apply a deterministic, value-changing edit to field `i` (0..5).
fn edit_field(issue: &mut Issue, i: usize) {
    match i {
        0 => issue.title.push_str("-edit"),
        1 => issue.body.push_str("-edit"),
        2 => {
            issue.state = if issue.state == "open" {
                "closed"
            } else {
                "open"
            }
            .to_string()
        }
        3 => issue.assignees.push(MemberId::new("added")),
        _ => issue.labels.push("added".to_string()),
    }
}

fn ser(objects: &ObjectStore, issue: &Issue) -> gix_hash::ObjectId {
    facet_git_tree::serialize_into(issue, objects).unwrap()
}

fn de(objects: &ObjectStore, tree: gix_hash::ObjectId) -> Issue {
    facet_git_tree::deserialize(&tree, objects).unwrap()
}

proptest! {
    /// Each side changes a *disjoint* set of fields, so the merge must fold
    /// both sets in with no conflict — the concrete meaning of "schema-aware
    /// three-way merge over the typed tree" (`sync.divergence-merge`): the
    /// merged entity carries every field's winning value, resolved per field.
    // @relation(sync.divergence-merge, scope=function, role=Verifies)
    #[test]
    fn disjoint_field_edits_merge_field_by_field(
        base in issue_strategy(),
        owners in prop::collection::vec(0u8..3, 5),
    ) {
        let objects = ObjectStore::default();
        let mut ours = base.clone();
        let mut theirs = base.clone();
        let mut expected = base.clone();
        for (i, &owner) in owners.iter().enumerate() {
            match owner {
                1 => { edit_field(&mut ours, i); edit_field(&mut expected, i); }
                2 => { edit_field(&mut theirs, i); edit_field(&mut expected, i); }
                _ => {}
            }
        }

        let b = ser(&objects, &base);
        let o = ser(&objects, &ours);
        let t = ser(&objects, &theirs);

        let merged = three_way(&objects, Some(b), o, t).unwrap();
        let tree = merged.tree().expect("disjoint edits never conflict");
        prop_assert_eq!(de(&objects, tree), expected);
    }

    /// Both sides change the *same* scalar field to different values: no
    /// content-addressed pick is possible, so the merge must report that
    /// field as a conflict rather than silently choose one.
    // @relation(sync.divergence-merge, scope=function, role=Verifies)
    #[test]
    fn same_field_divergent_edits_conflict(base in issue_strategy()) {
        let objects = ObjectStore::default();
        let mut ours = base.clone();
        ours.title.push_str("-ours");
        let mut theirs = base.clone();
        theirs.title.push_str("-theirs");

        let b = ser(&objects, &base);
        let o = ser(&objects, &ours);
        let t = ser(&objects, &theirs);

        let merged = three_way(&objects, Some(b), o, t).unwrap();
        prop_assert_eq!(merged, Merge::Conflict(vec!["title".into()]));
    }

    /// The merge is commutative in its two sides: swapping `ours` and
    /// `theirs` yields the identical clean tree, or the identical conflict
    /// set. A resolution that depended on argument order would silently
    /// disagree with itself across two machines.
    // @relation(sync.divergence-merge, scope=function, role=Verifies)
    #[test]
    fn merge_is_commutative(
        base in issue_strategy(),
        ours in issue_strategy(),
        theirs in issue_strategy(),
    ) {
        let objects = ObjectStore::default();
        let b = ser(&objects, &base);
        let o = ser(&objects, &ours);
        let t = ser(&objects, &theirs);

        let forward = three_way(&objects, Some(b), o, t).unwrap();
        let backward = three_way(&objects, Some(b), t, o).unwrap();

        match (forward, backward) {
            (Merge::Clean(x), Merge::Clean(y)) => prop_assert_eq!(x, y),
            (Merge::Conflict(a), Merge::Conflict(bb)) => prop_assert_eq!(a, bb),
            (f, bk) => prop_assert!(false, "clean-ness must match: {:?} vs {:?}", f, bk),
        }
    }

    /// A side that did not move is a no-op: `three_way(base, base, theirs)`
    /// adopts `theirs` wholesale — the field-level analogue of a
    /// fast-forward, and the trivial-merge case adoption relies on.
    // @relation(sync.divergence-merge, scope=function, role=Verifies)
    #[test]
    fn one_sided_change_adopts_the_other(
        base in issue_strategy(),
        theirs in issue_strategy(),
    ) {
        let objects = ObjectStore::default();
        let b = ser(&objects, &base);
        let t = ser(&objects, &theirs);

        prop_assert_eq!(three_way(&objects, Some(b), b, t).unwrap(), Merge::Clean(t));
        prop_assert_eq!(three_way(&objects, Some(b), b, b).unwrap(), Merge::Clean(b));
    }
}
