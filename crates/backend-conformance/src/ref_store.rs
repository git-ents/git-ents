//! Property functions for [`git_backend::RefStore`] implementations — the
//! same suite run against every backend (`docs/scale-out.adoc`, "Storage
//! traits" / WS2).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "assertion helpers for a conformance suite, not application code"
)]

use std::sync::Arc;

use git_backend::{Expected, RefEdit, RefName, RefStore, TxOutcome};
use gix_hash::ObjectId;

use crate::FixtureOids;

/// Run every [`RefStore`] property against a fresh backend built by `mk`.
/// Each property gets its own fresh backend instance (a fresh call to
/// `mk`) so one property's writes never leak into another's assertions.
pub fn ref_store_properties<S>(mk: impl Fn() -> S)
where
    S: RefStore + FixtureOids + 'static,
{
    multi_ref_all_or_nothing(&mk());
    prefix_iteration_consistency(&mk());
    reflog_records_transactions(&mk());
    watch_loss_tolerance(&mk());
    multi_ref_cas_concurrent_conflict(&mk);
}

/// One failing edit in a multi-ref transaction rejects the whole batch —
/// no partial application (`docs/scale-out.adoc`, "RefStore": "Multi-ref
/// compare-and-swap is in the contract").
pub fn multi_ref_all_or_nothing<S: RefStore + FixtureOids>(store: &S) {
    let mut oids = store.fixture_oids(2).into_iter();
    let new_oid = oids.next().expect("first oid");
    let mismatched_oid = oids.next().expect("second oid");

    let a = RefName::new("refs/conformance/all-or-nothing/a");
    let b = RefName::new("refs/conformance/all-or-nothing/b");

    // `b`'s precondition already fails (it doesn't exist), so `a` must not
    // apply either, even though its own precondition holds.
    let edits = [
        RefEdit {
            name: a.clone(),
            expected: Expected::MustNotExist,
            new: Some(new_oid),
        },
        RefEdit {
            name: b.clone(),
            expected: Expected::MustExistAndMatch(mismatched_oid),
            new: Some(new_oid),
        },
    ];
    let outcome = store.transaction(&edits).expect("transaction");
    assert!(
        matches!(outcome, TxOutcome::Rejected { .. }),
        "a batch with one failing precondition must reject the whole transaction"
    );
    assert_eq!(
        store.get(&a).expect("get a"),
        None,
        "a rejected batch must not partially apply — a's own edit had a valid precondition but must not have landed"
    );
    assert_eq!(store.get(&b).expect("get b"), None);
}

/// `iter_prefix` agrees with `get` after transactions land — additions and
/// deletions alike.
pub fn prefix_iteration_consistency<S: RefStore + FixtureOids>(store: &S) {
    let mut oids = store.fixture_oids(2).into_iter();
    let inside_oid = oids.next().expect("first oid");
    let outside_oid = oids.next().expect("second oid");

    let inside = RefName::new("refs/conformance/prefix/inside");
    let outside = RefName::new("refs/conformance/other/outside");
    let prefix = RefName::new("refs/conformance/prefix/");

    store
        .transaction(&[
            RefEdit {
                name: inside.clone(),
                expected: Expected::MustNotExist,
                new: Some(inside_oid),
            },
            RefEdit {
                name: outside.clone(),
                expected: Expected::MustNotExist,
                new: Some(outside_oid),
            },
        ])
        .expect("transaction");

    let listed: Vec<_> = store
        .iter_prefix(&prefix)
        .expect("iter_prefix")
        .map(|item| item.expect("ref entry"))
        .collect();
    assert_eq!(
        listed,
        vec![(inside.clone(), inside_oid)],
        "iter_prefix must list exactly the refs under the prefix, agreeing with get"
    );
    assert_eq!(store.get(&inside).expect("get inside"), Some(inside_oid));

    // Delete it via a transaction; iter_prefix must reflect the deletion.
    store
        .transaction(&[RefEdit {
            name: inside.clone(),
            expected: Expected::MustExistAndMatch(inside_oid),
            new: None,
        }])
        .expect("delete transaction");
    let listed_after_delete: Vec<_> = store
        .iter_prefix(&prefix)
        .expect("iter_prefix after delete")
        .collect();
    assert!(
        listed_after_delete.is_empty(),
        "iter_prefix must not list a ref deleted by a transaction"
    );
    assert_eq!(store.get(&inside).expect("get after delete"), None);
}

/// A transaction appends to the ref's log.
pub fn reflog_records_transactions<S: RefStore + FixtureOids>(store: &S) {
    let oid = store.fixture_oids(1).into_iter().next().expect("oid");
    let name = RefName::new("refs/conformance/reflog/probe");
    store
        .transaction(&[RefEdit {
            name: name.clone(),
            expected: Expected::MustNotExist,
            new: Some(oid),
        }])
        .expect("transaction");

    let entries: Vec<_> = store
        .log(&name)
        .expect("log")
        .map(|entry| entry.expect("log entry"))
        .collect();
    assert!(
        !entries.is_empty(),
        "a transaction must append to the ref's log"
    );
    let latest = entries.first().expect("at least one entry");
    assert_eq!(latest.new, Some(oid));
}

/// `watch` is a hint only. Killing the event stream mid-flight (dropping it
/// before it delivers anything) must never lose the underlying ref state:
/// transactions still land and later reads are still correct
/// (`docs/scale-out.adoc`, "RefStore"). Queue-table recovery on reconnect
/// is a cloud backend's own concern; this asserts the backend-independent
/// half — no *state* is lost when the channel drops.
pub fn watch_loss_tolerance<S: RefStore + FixtureOids>(store: &S) {
    let mut oids = store.fixture_oids(2).into_iter();
    let first_oid = oids.next().expect("first oid");
    let second_oid = oids.next().expect("second oid");

    let prefix = RefName::new("refs/conformance/watch-loss/");
    let first = RefName::new("refs/conformance/watch-loss/probe-1");
    let second = RefName::new("refs/conformance/watch-loss/probe-2");

    // Open a watcher, then drop it immediately — simulating a connection
    // that dies mid-flight before it delivers anything.
    let watcher = store.watch(&prefix).expect("watch");
    drop(watcher);

    store
        .transaction(&[RefEdit {
            name: first.clone(),
            expected: Expected::MustNotExist,
            new: Some(first_oid),
        }])
        .expect("transaction after dropping the watcher");
    assert_eq!(
        store.get(&first).expect("get after watch drop"),
        Some(first_oid),
        "a transaction must land correctly even though its watcher was dropped before delivery"
    );

    // A fresh watch opened after the drop must still see subsequent
    // changes: the earlier drop must not have wedged the watch mechanism.
    let watcher = store.watch(&prefix).expect("watch again");
    store
        .transaction(&[RefEdit {
            name: second.clone(),
            expected: Expected::MustNotExist,
            new: Some(second_oid),
        }])
        .expect("second transaction");
    // Best-effort: the hint arriving is not required (that's the whole
    // point of "hint only"); re-reading afterward must be correct
    // regardless of whether it did.
    let _hint = watcher.recv_timeout(std::time::Duration::from_millis(200));
    assert_eq!(
        store.get(&second).expect("get after second transaction"),
        Some(second_oid)
    );
}

/// Concurrent, conflicting multi-ref transactions: exactly one wins, no
/// partial application, and every loser is `Rejected` rather than causing
/// corruption (`docs/scale-out.adoc`, "RefStore").
pub fn multi_ref_cas_concurrent_conflict<S>(mk: &impl Fn() -> S)
where
    S: RefStore + FixtureOids + 'static,
{
    const CONTENDERS: usize = 8;

    let store = Arc::new(mk());
    let a = RefName::new("refs/conformance/cas-race/a");
    let b = RefName::new("refs/conformance/cas-race/b");

    let mut oids = store.fixture_oids(CONTENDERS.saturating_add(1)).into_iter();
    let baseline = oids.next().expect("baseline oid");
    let candidates: Vec<ObjectId> = oids.collect();

    store
        .transaction(&[
            RefEdit {
                name: a.clone(),
                expected: Expected::MustNotExist,
                new: Some(baseline),
            },
            RefEdit {
                name: b.clone(),
                expected: Expected::MustNotExist,
                new: Some(baseline),
            },
        ])
        .expect("seed transaction");

    let handles: Vec<_> = candidates
        .into_iter()
        .map(|candidate| {
            let store = Arc::clone(&store);
            let (a, b) = (a.clone(), b.clone());
            std::thread::spawn(move || {
                let outcome = store
                    .transaction(&[
                        RefEdit {
                            name: a,
                            expected: Expected::MustExistAndMatch(baseline),
                            new: Some(candidate),
                        },
                        RefEdit {
                            name: b,
                            expected: Expected::MustExistAndMatch(baseline),
                            new: Some(candidate),
                        },
                    ])
                    .expect("contender transaction");
                (outcome, candidate)
            })
        })
        .collect();

    let outcomes: Vec<(TxOutcome, ObjectId)> = handles
        .into_iter()
        .map(|handle| handle.join().expect("contender thread panicked"))
        .collect();

    let applied = outcomes
        .iter()
        .filter(|(outcome, _)| matches!(outcome, TxOutcome::Applied))
        .count();
    assert_eq!(
        applied, 1,
        "exactly one conflicting concurrent multi-ref transaction must win the CAS race"
    );
    let rejected = outcomes
        .iter()
        .filter(|(outcome, _)| matches!(outcome, TxOutcome::Rejected { .. }))
        .count();
    assert_eq!(
        rejected,
        CONTENDERS.saturating_sub(1),
        "every losing contender must be Rejected, not corrupted or silently dropped"
    );

    let winner = outcomes
        .iter()
        .find_map(|(outcome, candidate)| {
            matches!(outcome, TxOutcome::Applied).then_some(*candidate)
        })
        .expect("exactly one applied outcome");

    let final_a = store.get(&a).expect("get a after race");
    let final_b = store.get(&b).expect("get b after race");
    assert_eq!(
        final_a,
        Some(winner),
        "ref a must reflect the single winning multi-ref transaction"
    );
    assert_eq!(
        final_b,
        Some(winner),
        "ref b must move together with a — no partial application under concurrency"
    );
}
