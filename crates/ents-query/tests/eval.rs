//! Evaluation integration tests on a synthetic repo: the
//! staged-pipeline and fan-in composition idioms handled incrementally
//! (the Phase 3 → 4 gate criterion), the exclusion idiom including a
//! subtrahend shrink, the work set, and the generation-number read
//! bound of `query.incremental`.

#![expect(
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "test code: fixture indexing panics are test failures"
)]

use std::collections::BTreeSet;

use ents_model::Status;
use ents_query::{Evaluator, Query, Transition};
use ents_testutil::{
    CountingFind, MemRefStore, ObjectStore, advance_ref, empty_tree, record_result,
};
use gix_hash::ObjectId;
use gix_ref_store::RefStoreRead as _;
use rstest::rstest;

fn parse(input: &str) -> Query {
    input.parse().expect("valid query in test")
}

fn short(oid: ObjectId) -> String {
    oid.to_string().get(..12).expect("40 hex chars").to_owned()
}

fn set(oids: &[ObjectId]) -> BTreeSet<ObjectId> {
    oids.iter().copied().collect()
}

/// The transition a fixture mutation just performed, reconstructed from
/// the refname and its old/new tips.
fn transition(name: &str, old: Option<ObjectId>, new: Option<ObjectId>) -> Transition {
    Transition {
        name: name.try_into().expect("valid refname"),
        old,
        new,
    }
}

/// Record a result and return the transition that landed it.
fn record(
    refs: &MemRefStore,
    objects: &ObjectStore,
    effect: &str,
    tested: ObjectId,
    status: Status,
    seconds: i64,
) -> Transition {
    let short = short(tested);
    let tip = record_result(refs, objects, effect, &short, status, None, seconds);
    transition(
        &format!("refs/meta/results/{effect}/{short}"),
        None,
        Some(tip),
    )
}

// ---------------------------------------------------------------------
// The staged-pipeline idiom, both transition directions.
// ---------------------------------------------------------------------

#[rstest]
// @relation(query.incremental, query.results, scope=function, role=Verifies)
fn staged_pipeline_fires_when_the_result_lands() {
    let refs = MemRefStore::default();
    let objects = ObjectStore::default();
    let commits = advance_ref(&refs, &objects, "refs/heads/main", 3, 100);
    let trigger = parse("rev(refs/heads/main) & results(unit, pass)");
    let evaluator = Evaluator::new(&refs, &objects);

    // Advancing main alone enters nothing: no unit results yet.
    let advance = transition("refs/heads/main", None, Some(commits[2]));
    assert!(
        evaluator
            .entry_set(&trigger, &advance)
            .expect("evaluates")
            .is_empty()
    );

    // A passing unit result for the middle commit: exactly it enters.
    let landed = record(&refs, &objects, "unit", commits[1], Status::Pass, 300);
    assert_eq!(
        evaluator.entry_set(&trigger, &landed).expect("evaluates"),
        set(&[commits[1]])
    );

    // A failing unit result for the tip enters nothing.
    let failed = record(&refs, &objects, "unit", commits[2], Status::Fail, 310);
    assert!(
        evaluator
            .entry_set(&trigger, &failed)
            .expect("evaluates")
            .is_empty()
    );
}

#[rstest]
// @relation(query.incremental, scope=function, role=Verifies)
fn staged_pipeline_fires_when_the_rev_side_arrives_second() {
    let refs = MemRefStore::default();
    let objects = ObjectStore::default();
    // The commit exists on a side branch, its result is already
    // recorded, and only then does main advance to include it.
    let commits = advance_ref(&refs, &objects, "refs/heads/dev", 2, 100);
    record(&refs, &objects, "unit", commits[1], Status::Pass, 200);

    let trigger = parse("rev(refs/heads/main) & results(unit, pass)");
    let evaluator = Evaluator::new(&refs, &objects);

    refs.set_str("refs/heads/main", commits[1]);
    let advance = transition("refs/heads/main", None, Some(commits[1]));
    // Both dev commits enter rev(main); only the tested one has a pass.
    assert_eq!(
        evaluator.entry_set(&trigger, &advance).expect("evaluates"),
        set(&[commits[1]])
    );
}

// ---------------------------------------------------------------------
// The fan-in idiom: fires when the last prerequisite lands, in
// whichever order the underlying refs moved.
// ---------------------------------------------------------------------

#[rstest]
#[case::a_then_b(true)]
#[case::b_then_a(false)]
// @relation(query.incremental, query.results, scope=function, role=Verifies)
fn fan_in_fires_once_when_the_last_prerequisite_lands(#[case] a_first: bool) {
    let refs = MemRefStore::default();
    let objects = ObjectStore::default();
    let commits = advance_ref(&refs, &objects, "refs/heads/main", 1, 100);
    let tested = commits[0];
    let trigger = parse("results(unit, pass) & results(integ, pass)");
    let evaluator = Evaluator::new(&refs, &objects);

    let (first, second) = if a_first {
        ("unit", "integ")
    } else {
        ("integ", "unit")
    };

    let first_landing = record(&refs, &objects, first, tested, Status::Pass, 200);
    assert!(
        evaluator
            .entry_set(&trigger, &first_landing)
            .expect("evaluates")
            .is_empty(),
        "one prerequisite alone must not fire"
    );

    let second_landing = record(&refs, &objects, second, tested, Status::Pass, 210);
    assert_eq!(
        evaluator
            .entry_set(&trigger, &second_landing)
            .expect("evaluates"),
        set(&[tested]),
        "the last prerequisite fires the fan-in"
    );
}

// ---------------------------------------------------------------------
// The exclusion idiom, including entry via a shrinking subtrahend.
// ---------------------------------------------------------------------

#[rstest]
// @relation(query.set-ops, query.incremental, scope=function, role=Verifies)
fn exclusion_skips_wip_branches_and_reenters_on_subtrahend_shrink() {
    let refs = MemRefStore::default();
    let objects = ObjectStore::default();
    let main = advance_ref(&refs, &objects, "refs/heads/main", 2, 100);
    let query = parse("rev(refs/heads/*) - rev(refs/heads/wip/*)");
    let evaluator = Evaluator::new(&refs, &objects);

    // A wip branch on top of main: its new commit enters both sides at
    // once, so it never enters the difference.
    let wip = advance_ref(&refs, &objects, "refs/heads/wip/x", 1, 200);
    let wip_advance = transition("refs/heads/wip/x", None, Some(wip[0]));
    assert!(
        evaluator
            .entry_set(&query, &wip_advance)
            .expect("evaluates")
            .is_empty()
    );

    // But main's own commits are in the difference.
    assert_eq!(
        evaluator.eval(&query).expect("evaluates"),
        set(&[main[0], main[1]])
    );

    // Deleting the wip branch shrinks the subtrahend: its commit is
    // still reachable from... nothing. Re-point wip at main's tip
    // first, so main's tip is temporarily excluded, then delete.
    refs.set_str("refs/heads/wip/x", main[1]);
    let repoint = transition("refs/heads/wip/x", Some(wip[0]), Some(main[1]));
    assert!(
        evaluator
            .entry_set(&query, &repoint)
            .expect("evaluates")
            .is_empty(),
        "pointing wip at existing commits only removes from the difference"
    );
    assert_eq!(evaluator.eval(&query).expect("evaluates"), BTreeSet::new());

    let wip_name: gix::refs::FullName = "refs/heads/wip/x".try_into().expect("valid");
    refs.remove(wip_name.as_ref());
    let deletion = transition("refs/heads/wip/x", Some(main[1]), None);
    // The subtrahend shrank: main's commits re-enter the difference.
    assert_eq!(
        evaluator.entry_set(&query, &deletion).expect("evaluates"),
        set(&[main[0], main[1]])
    );
}

// ---------------------------------------------------------------------
// The work set: trigger − results(self, any).
// ---------------------------------------------------------------------

#[rstest]
// @relation(query.workset, scope=function, role=Verifies)
fn work_set_subtracts_any_recorded_status_by_refname_scan() {
    let refs = MemRefStore::default();
    let objects = ObjectStore::default();
    let first = advance_ref(&refs, &objects, "refs/heads/main", 1, 100);
    let trigger = parse("rev(refs/heads/main)");
    let evaluator = Evaluator::new(&refs, &objects);

    // Advance by two; an *error* result for the first new commit is
    // already recorded (a terminal infrastructure verdict counts as
    // recorded — the obligation is discharged, `query.workset`).
    let new = advance_ref(&refs, &objects, "refs/heads/main", 2, 200);
    record(&refs, &objects, "unit", new[0], Status::Error, 250);

    let advance = transition("refs/heads/main", Some(first[0]), Some(new[1]));
    assert_eq!(
        evaluator.entry_set(&trigger, &advance).expect("evaluates"),
        set(&[new[0], new[1]]),
        "the trigger itself knows nothing about results"
    );
    assert_eq!(
        evaluator
            .work_set("unit", &trigger, &advance)
            .expect("evaluates"),
        set(&[new[1]]),
        "the effect's own results ref is the sole materialization marker"
    );
}

#[rstest]
// @relation(query.workset, query.monotone, scope=function, role=Verifies)
fn outstanding_reconstructs_the_obligation_set_from_repository_state() {
    let refs = MemRefStore::default();
    let objects = ObjectStore::default();
    let commits = advance_ref(&refs, &objects, "refs/heads/main", 3, 100);
    record(&refs, &objects, "unit", commits[0], Status::Pass, 200);
    record(&refs, &objects, "unit", commits[2], Status::Fail, 210);

    let trigger = parse("rev(refs/heads/main)");
    let evaluator = Evaluator::new(&refs, &objects);
    // No pipeline state anywhere: the outstanding set falls out of ref
    // state alone — exactly what a boot-time reconciliation scan needs.
    assert_eq!(
        evaluator.outstanding("unit", &trigger).expect("evaluates"),
        set(&[commits[1]])
    );
}

// ---------------------------------------------------------------------
// meta() entry, and monotone non-retraction.
// ---------------------------------------------------------------------

#[rstest]
// @relation(query.meta, query.monotone, scope=function, role=Verifies)
fn meta_tips_enter_and_old_tips_are_not_retracted() {
    let refs = MemRefStore::default();
    let objects = ObjectStore::default();
    let query = parse("meta(refs/meta/issues/*)");
    let evaluator = Evaluator::new(&refs, &objects);

    let tip1 = record_result(&refs, &objects, "unused", "seed", Status::Pass, None, 90);
    let _ = tip1; // keep the results namespace non-empty and irrelevant

    let tree = empty_tree(&objects);
    let write = |parents: Vec<ObjectId>, seconds: i64| {
        ents_testutil::write_commit(
            &objects,
            &ents_testutil::CommitSpec {
                tree,
                parents,
                message: format!("issue mutation at {seconds}"),
                seconds,
            },
            None,
        )
    };
    let first = write(vec![], 100);
    refs.set_str("refs/meta/issues/7", first);
    let created = transition("refs/meta/issues/7", None, Some(first));
    assert_eq!(
        evaluator.entry_set(&query, &created).expect("evaluates"),
        set(&[first])
    );

    let second = write(vec![first], 110);
    refs.set_str("refs/meta/issues/7", second);
    let advanced = transition("refs/meta/issues/7", Some(first), Some(second));
    // Only the new tip enters; the old tip leaving the set retracts
    // nothing — it simply stops being a tip.
    assert_eq!(
        evaluator.entry_set(&query, &advanced).expect("evaluates"),
        set(&[second])
    );
}

#[rstest]
// @relation(query.monotone, scope=function, role=Verifies)
fn a_force_push_shrinks_the_set_without_reentering_survivors() {
    let refs = MemRefStore::default();
    let objects = ObjectStore::default();
    let commits = advance_ref(&refs, &objects, "refs/heads/main", 3, 100);
    let query = parse("rev(refs/heads/main)");
    let evaluator = Evaluator::new(&refs, &objects);

    // Force main back to its first commit: the set shrinks, nothing
    // enters, nothing is retracted.
    refs.set_str("refs/heads/main", commits[0]);
    let force = transition("refs/heads/main", Some(commits[2]), Some(commits[0]));
    assert!(
        evaluator
            .entry_set(&query, &force)
            .expect("evaluates")
            .is_empty()
    );

    // Re-advancing over the same commits re-enters them: whether they
    // fire again is the work set's job, not the entry set's — results
    // already written keep them discharged.
    refs.set_str("refs/heads/main", commits[2]);
    let restore = transition("refs/heads/main", Some(commits[0]), Some(commits[2]));
    assert_eq!(
        evaluator.entry_set(&query, &restore).expect("evaluates"),
        set(&[commits[1], commits[2]])
    );
}

// ---------------------------------------------------------------------
// The generation-number bound (`query.incremental`).
// ---------------------------------------------------------------------

#[rstest]
// @relation(query.incremental, scope=function, role=Verifies)
fn entry_after_a_one_commit_advance_reads_a_bounded_frontier() {
    const HISTORY: usize = 300;
    const READ_BUDGET: usize = 25;

    let refs = MemRefStore::default();
    let objects = ObjectStore::default();
    let commits = advance_ref(&refs, &objects, "refs/heads/main", HISTORY, 1_000);
    let tip = *commits.last().expect("non-empty");

    let counting = CountingFind::new(&objects);
    let evaluator = Evaluator::new(&refs, &counting);
    let query = parse("rev(refs/heads/main) | results(unit, pass)");

    // Warm the evaluator the way a long-lived receive process is warm:
    // one reconciliation pass caches commit structure for the history.
    let full = evaluator.eval(&query).expect("evaluates");
    assert_eq!(full.len(), HISTORY);
    let warm_reads = counting.reads();
    assert!(warm_reads >= HISTORY, "the warm-up walk pays for history");

    // One commit lands. The entry set must be computed from the
    // frontier, not by re-walking three hundred commits.
    let new = advance_ref(&refs, &objects, "refs/heads/main", 1, 2_000);
    counting.reset();
    let entered = evaluator
        .entry_set(
            &query,
            &transition("refs/heads/main", Some(tip), Some(new[0])),
        )
        .expect("evaluates");
    assert_eq!(entered, set(&[new[0]]));
    assert!(
        counting.reads() <= READ_BUDGET,
        "a one-commit advance read {} objects; the frontier bound allows {}",
        counting.reads(),
        READ_BUDGET
    );
}

#[rstest]
// @relation(query.footprint, query.incremental, scope=function, role=Verifies)
fn transitions_outside_the_footprint_are_free() {
    let refs = MemRefStore::default();
    let objects = ObjectStore::default();
    let commits = advance_ref(&refs, &objects, "refs/heads/main", 2, 100);
    advance_ref(&refs, &objects, "refs/heads/dev", 2, 200);

    let counting = CountingFind::new(&objects);
    let evaluator = Evaluator::new(&refs, &counting);
    let query = parse("rev(refs/heads/main)");

    let dev_tip = refs
        .get("refs/heads/dev".try_into().expect("valid"))
        .ok()
        .flatten();
    let unrelated = transition("refs/heads/dev", None, dev_tip);
    assert!(
        evaluator
            .entry_set(&query, &unrelated)
            .expect("evaluates")
            .is_empty()
    );
    assert_eq!(
        counting.reads(),
        0,
        "a non-matching footprint must short-circuit before any object read"
    );
    let _ = commits;
}
