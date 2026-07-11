//! Property test for monotone, entry-only semantics (`query.monotone`)
//! and incremental-equals-full evaluation (`query.incremental`): random
//! synthetic ref histories — advances, force-pushes, deletions, result
//! recordings — checked after every transition against an independent
//! naive oracle. The evaluator's incremental entry set must equal the
//! oracle's `full(after) − full(before)` for every query whose static
//! footprint the transitioned ref touches, and the empty set otherwise
//! (`query.footprint`, `query.results`): a `results()` atom's entry set
//! cannot be moved by an unrelated ref's reachability, only by its own
//! results refs. The work set must equal the entry set minus recorded
//! prefixes.

#![expect(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::unreachable,
    reason = "test code: fixture indexing panics are test failures"
)]

use std::collections::{HashMap, HashSet};

use ents_model::{Member, Provenance, Status};
use ents_query::{Evaluator, Query, Transition};
use ents_testutil::{MemRefStore, ObjectStore, advance_ref, record_result, write_member};
use gix_hash::ObjectId;
use gix_object::{CommitRef, Find as _};
use gix_ref_store::RefStoreRead as _;
use proptest::prelude::*;

const BRANCHES: [&str; 3] = ["refs/heads/main", "refs/heads/dev", "refs/heads/wip/x"];
const EFFECTS: [&str; 2] = ["unit", "integ"];
const STATUSES: [Status; 3] = [Status::Pass, Status::Fail, Status::Error];
const MEMBERS: [&str; 2] = ["alice", "bob"];

/// One randomized history operation.
#[derive(Debug, Clone)]
enum Op {
    Advance {
        branch: usize,
        count: usize,
    },
    ForceTo {
        branch: usize,
        commit: usize,
    },
    Delete {
        branch: usize,
    },
    Record {
        effect: usize,
        commit: usize,
        status: usize,
        short_len: usize,
    },
    Member {
        who: usize,
    },
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        (0..3usize, 1..3usize).prop_map(|(branch, count)| Op::Advance { branch, count }),
        (0..3usize, 0..64usize).prop_map(|(branch, commit)| Op::ForceTo { branch, commit }),
        (0..3usize).prop_map(|branch| Op::Delete { branch }),
        (0..2usize, 0..64usize, 0..3usize, 7..12usize).prop_map(
            |(effect, commit, status, short_len)| Op::Record {
                effect,
                commit,
                status,
                short_len
            }
        ),
        (0..2usize).prop_map(|who| Op::Member { who }),
    ]
}

// ---------------------------------------------------------------------
// The naive oracle: full evaluation from first principles.
// ---------------------------------------------------------------------

type Refs = HashMap<String, ObjectId>;

fn snapshot(refs: &MemRefStore) -> Refs {
    refs.iter_prefix("refs/")
        .expect("iterable")
        .map(|entry| {
            let (name, oid) = entry.expect("readable");
            (name.as_bstr().to_string(), oid)
        })
        .collect()
}

fn parents(objects: &ObjectStore, oid: ObjectId) -> Vec<ObjectId> {
    let mut buf = Vec::new();
    let data = objects
        .try_find(&oid, &mut buf)
        .expect("readable")
        .expect("present");
    CommitRef::from_bytes(data.data, oid.kind())
        .expect("a commit")
        .parents()
        .collect()
}

fn reach(objects: &ObjectStore, tips: impl IntoIterator<Item = ObjectId>) -> HashSet<ObjectId> {
    let mut seen = HashSet::new();
    let mut queue: Vec<ObjectId> = tips.into_iter().collect();
    while let Some(oid) = queue.pop() {
        if seen.insert(oid) {
            queue.extend(parents(objects, oid));
        }
    }
    seen
}

fn reach_glob(objects: &ObjectStore, refs: &Refs, prefix: &str) -> HashSet<ObjectId> {
    reach(
        objects,
        refs.iter()
            .filter(|(name, _)| name.starts_with(prefix) && !name.starts_with("refs/meta/"))
            .map(|(_, oid)| *oid),
    )
}

/// Recorded shorts for one effect whose status satisfies `want`
/// (`None` = any), read back through facet like the evaluator does.
fn recorded(objects: &ObjectStore, refs: &Refs, effect: &str, want: Option<Status>) -> Vec<String> {
    let prefix = format!("refs/meta/results/{effect}/");
    refs.iter()
        .filter_map(|(name, tip)| {
            let short = name.strip_prefix(&prefix)?;
            let mut buf = Vec::new();
            let data = objects.try_find(tip, &mut buf).expect("readable")?;
            let tree = CommitRef::from_bytes(data.data, tip.kind())
                .expect("commit")
                .tree();
            let status: Status = facet_git_tree::deserialize(&tree, objects).expect("status tree");
            (want.is_none() || want == Some(status)).then(|| short.to_owned())
        })
        .collect()
}

fn universe(objects: &ObjectStore, refs: &Refs) -> HashSet<ObjectId> {
    reach(objects, refs.values().copied())
}

fn by_prefix(universe: &HashSet<ObjectId>, shorts: &[String]) -> HashSet<ObjectId> {
    universe
        .iter()
        .copied()
        .filter(|oid| {
            let hex = oid.to_string();
            shorts.iter().any(|s| hex.starts_with(s.as_str()))
        })
        .collect()
}

/// The six checked queries and their independent full evaluations.
fn oracle(objects: &ObjectStore, refs: &Refs, query_index: usize) -> HashSet<ObjectId> {
    let main = reach(objects, refs.get("refs/heads/main").copied());
    match query_index {
        0 => main,
        1 => {
            let heads = reach_glob(objects, refs, "refs/heads/");
            let wip = reach_glob(objects, refs, "refs/heads/wip/");
            heads.difference(&wip).copied().collect()
        }
        2 => {
            let u = universe(objects, refs);
            let pass = by_prefix(&u, &recorded(objects, refs, "unit", Some(Status::Pass)));
            main.intersection(&pass).copied().collect()
        }
        3 => {
            let u = universe(objects, refs);
            let unit = by_prefix(&u, &recorded(objects, refs, "unit", Some(Status::Pass)));
            let integ = by_prefix(&u, &recorded(objects, refs, "integ", Some(Status::Pass)));
            unit.intersection(&integ).copied().collect()
        }
        4 => {
            let dev = reach(objects, refs.get("refs/heads/dev").copied());
            main.union(&dev).copied().collect()
        }
        // `meta(refs/meta/member/*)` (`query.meta`): the tip commits of
        // matching author-written meta-refs directly, never a
        // reachability closure — computed independently of both
        // `Query::footprint` and the evaluator's `meta_tips`.
        5 => refs
            .iter()
            .filter(|(name, _)| name.starts_with("refs/meta/member/"))
            .map(|(_, oid)| *oid)
            .collect(),
        _ => unreachable!("six queries"),
    }
}

const QUERIES: [&str; 6] = [
    "rev(refs/heads/main)",
    "rev(refs/heads/*) - rev(refs/heads/wip/*)",
    "rev(refs/heads/main) & results(unit, pass)",
    "results(unit, pass) & results(integ, pass)",
    "rev(refs/heads/main) | rev(refs/heads/dev)",
    "meta(refs/meta/member/*)",
];

/// Each of the six queries' static ref-footprint (`query.footprint`),
/// hand-written independently of `Query::footprint` — this is what the
/// oracle checks against, not a call into the thing under test.
///
/// A `results()` atom's footprint is its own results namespace, nothing
/// else (`query.results`): a transition outside a query's footprint,
/// such as deleting and recreating an unrelated branch, is a non-event
/// for that query's entry set, even though it can change which commits
/// `oracle`'s shared reachable-universe helper currently sees (a
/// reconciliation-grade concern the diff below must not leak into
/// incremental entry).
fn footprint_touches(query_index: usize, moved: &str) -> bool {
    let results_of = |effect: &str| moved.starts_with(&format!("refs/meta/results/{effect}/"));
    match query_index {
        0 => moved == "refs/heads/main",
        1 => moved.starts_with("refs/heads/"),
        2 => moved == "refs/heads/main" || results_of("unit"),
        3 => results_of("unit") || results_of("integ"),
        4 => moved == "refs/heads/main" || moved == "refs/heads/dev",
        5 => moved.starts_with("refs/meta/member/"),
        _ => unreachable!("six queries"),
    }
}

// ---------------------------------------------------------------------
// The property.
// ---------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    // @relation(query.monotone, query.incremental, query.set-ops, query.workset, query.footprint, query.results, scope=function, role=Verifies)
    #[test]
    fn entry_sets_equal_the_oracle_diff_over_random_histories(
        ops in proptest::collection::vec(op_strategy(), 1..14)
    ) {
        let refs = MemRefStore::default();
        let objects = ObjectStore::default();
        let queries: Vec<Query> =
            QUERIES.iter().map(|q| q.parse().expect("valid")).collect();
        let evaluator = Evaluator::new(&refs, &objects);

        let mut commits: Vec<ObjectId> = Vec::new();
        let mut seconds = 1_000i64;

        for op in ops {
            let before = snapshot(&refs);

            // Apply the operation; `moved` is the transitioned refname.
            let moved: Option<String> = match op {
                Op::Advance { branch, count } => {
                    let name = BRANCHES[branch];
                    seconds += 100;
                    commits.extend(advance_ref(&refs, &objects, name, count, seconds));
                    Some(name.to_owned())
                }
                Op::ForceTo { branch, commit } => {
                    if commits.is_empty() {
                        continue;
                    }
                    let name = BRANCHES[branch];
                    let target = commits[commit % commits.len()];
                    refs.set_str(name, target);
                    Some(name.to_owned())
                }
                Op::Delete { branch } => {
                    let name = BRANCHES[branch];
                    let full: gix::refs::FullName = name.try_into().expect("valid");
                    refs.remove(full.as_ref());
                    Some(name.to_owned())
                }
                Op::Record { effect, commit, status, short_len } => {
                    if commits.is_empty() {
                        continue;
                    }
                    let tested = commits[commit % commits.len()];
                    let short = tested.to_string()
                        .get(..short_len)
                        .expect("40 hex chars")
                        .to_owned();
                    seconds += 100;
                    record_result(
                        &refs, &objects, EFFECTS[effect], &short,
                        STATUSES[status], None, seconds,
                    );
                    Some(format!("refs/meta/results/{}/{short}", EFFECTS[effect]))
                }
                Op::Member { who } => {
                    let id = MEMBERS[who];
                    seconds += 100;
                    let member = Member::new(format!("key-{id}"), Provenance::AdminRegistered);
                    write_member(&refs, &objects, id, &member, None, seconds);
                    Some(format!("refs/meta/member/{id}"))
                }
            };

            let after = snapshot(&refs);
            let Some(moved) = moved else { continue };
            let (old, new) = (before.get(&moved).copied(), after.get(&moved).copied());
            if old == new {
                continue; // a no-op transition denotes no frontier
            }
            let transition = Transition {
                name: moved.as_str().try_into().expect("valid"),
                old,
                new,
            };

            for (index, query) in queries.iter().enumerate() {
                // The hand-written mirror must agree with the real
                // `Query::footprint()` on every generated transition —
                // any silent divergence between the mirror's theory and
                // the implementation's derivation is a loud failure
                // here, not a false pass downstream.
                let touches = footprint_touches(index, &moved);
                prop_assert_eq!(
                    touches,
                    query.footprint().matches(transition.name.as_ref()),
                    "footprint mirror disagreement for {} on {:?}", QUERIES[index], moved
                );

                let full_before = oracle(&objects, &before, index);
                let full_after = oracle(&objects, &after, index);

                // Incremental entry == full(after) − full(before), but
                // only within the query's own footprint; a transition
                // outside it is a non-event no matter what the raw diff
                // of two full evaluations would suggest.
                let expected: std::collections::BTreeSet<ObjectId> =
                    if touches {
                        full_after.difference(&full_before).copied().collect()
                    } else {
                        std::collections::BTreeSet::new()
                    };
                let entered = evaluator
                    .entry_set(query, &transition)
                    .expect("evaluates");
                prop_assert_eq!(
                    &entered, &expected,
                    "entry mismatch for {} under {:?}", QUERIES[index], transition
                );

                // Full evaluation agrees with the oracle outright.
                let full = evaluator.eval(query).expect("evaluates");
                let oracle_after: std::collections::BTreeSet<ObjectId> =
                    full_after.iter().copied().collect();
                prop_assert_eq!(
                    &full, &oracle_after,
                    "full-eval mismatch for {}", QUERIES[index]
                );
            }

            // The work set: trigger − results(self, any), on the plain
            // rev trigger.
            let trigger = &queries[0];
            let entered = evaluator.entry_set(trigger, &transition).expect("evaluates");
            let unit_any = recorded(&objects, &after, "unit", None);
            let expected_work: std::collections::BTreeSet<ObjectId> = entered
                .iter()
                .copied()
                .filter(|oid| {
                    let hex = oid.to_string();
                    !unit_any.iter().any(|s| hex.starts_with(s.as_str()))
                })
                .collect();
            let work = evaluator
                .work_set("unit", trigger, &transition)
                .expect("evaluates");
            prop_assert_eq!(&work, &expected_work, "work-set mismatch");
        }
    }
}
