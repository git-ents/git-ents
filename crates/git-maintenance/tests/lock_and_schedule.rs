//! Advisory-lock serialization (two concurrent maintenance runs — one
//! runs, one skips) and threshold-driven scheduling
//! (`docs/scale-out.adoc`, WS9; the Postgres advisory-lock counterpart is
//! exercised in `refstore-postgres`'s docker-gated suite).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::unwrap_in_result,
    reason = "test assertions, not application code"
)]

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use git_backend::EffectDef;
use git_maintenance::lock::{FileMaintenanceLock, MaintenanceLock as _, run_exclusive};
use git_maintenance::schedule::{
    MaintenanceSink, Scheduler, Stats, Thresholds, schedule_maintenance,
};

#[test]
fn two_concurrent_maintenance_runs_serialize_one_skips() {
    let dir = tempfile::tempdir().unwrap();
    let lock_a = FileMaintenanceLock::for_repo(dir.path());
    let lock_b = FileMaintenanceLock::for_repo(dir.path());

    // While one run holds the lock, a concurrent run skips whole —
    // `run_exclusive` returns `None` without executing its work.
    let guard = lock_a.try_acquire().unwrap().expect("first acquisition");
    let ran = std::sync::atomic::AtomicBool::new(false);
    let outcome = run_exclusive(&lock_b, || {
        ran.store(true, Ordering::SeqCst);
        Ok(())
    })
    .unwrap();
    assert!(outcome.is_none(), "the second run must skip");
    assert!(!ran.load(Ordering::SeqCst), "skipped means never executed");

    // Release: the next run proceeds.
    drop(guard);
    let outcome = run_exclusive(&lock_b, || Ok(42)).unwrap();
    assert_eq!(outcome, Some(42));
}

#[test]
fn the_lock_wraps_the_whole_run() {
    let dir = tempfile::tempdir().unwrap();
    let lock_a = FileMaintenanceLock::for_repo(dir.path());
    let lock_b = FileMaintenanceLock::for_repo(dir.path());

    // From inside a run, a concurrent acquisition fails — the lock is
    // held for the run's full duration, not per phase.
    let outcome = run_exclusive(&lock_a, || {
        assert!(lock_b.try_acquire().unwrap().is_none());
        Ok(())
    })
    .unwrap();
    assert!(outcome.is_some());
    // And it is released once the run returns.
    assert!(lock_b.try_acquire().unwrap().is_some());
}

/// A sink recording every enqueue.
#[derive(Default)]
struct RecordingSink {
    enqueued: Mutex<Vec<(String, Vec<EffectDef>)>>,
}

impl MaintenanceSink for RecordingSink {
    fn enqueue(&self, repo_id: &str, effects: &[EffectDef]) -> git_backend::Result<()> {
        self.enqueued
            .lock()
            .unwrap()
            .push((repo_id.to_owned(), effects.to_vec()));
        Ok(())
    }
}

#[test]
fn schedule_maintenance_enqueues_all_four_effects_at_threshold() {
    let sink = RecordingSink::default();
    let thresholds = Thresholds {
        maintenance: 10,
        reachability: 10,
    };

    // Below threshold: nothing.
    let effects = schedule_maintenance(
        "repo",
        &Stats {
            ref_updates_since_last: 9,
        },
        &thresholds,
        &sink,
    )
    .unwrap();
    assert!(effects.is_empty());
    assert!(sink.enqueued.lock().unwrap().is_empty());

    // At threshold: GC, cache TTL, consolidation, and reachability
    // regeneration (WS6's `should_regenerate` trigger, scheduled here).
    let effects = schedule_maintenance(
        "repo",
        &Stats {
            ref_updates_since_last: 10,
        },
        &thresholds,
        &sink,
    )
    .unwrap();
    let names: Vec<&str> = effects.iter().map(|effect| effect.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            git_maintenance::schedule::GC_EFFECT,
            git_maintenance::schedule::CACHE_TTL_EFFECT,
            git_maintenance::schedule::CONSOLIDATION_EFFECT,
            git_reachability::maintenance::EFFECT_NAME,
        ]
    );
    let enqueued = sink.enqueued.lock().unwrap();
    assert_eq!(enqueued.len(), 1);
    let (repo, batch) = enqueued.first().unwrap();
    assert_eq!(repo, "repo");
    assert_eq!(batch.len(), 4);
}

#[test]
fn the_scheduler_accumulates_per_repo_and_resets_on_trigger() {
    let sink = std::sync::Arc::new(RecordingSink::default());
    let scheduler = Scheduler::new(
        Thresholds {
            maintenance: 5,
            reachability: 5,
        },
        sink.clone(),
    );

    // Accumulate across pushes; trigger only at the threshold.
    assert!(scheduler.note_ref_updates("a", 2).unwrap().is_empty());
    assert!(scheduler.note_ref_updates("a", 2).unwrap().is_empty());
    // A different repo's count is independent.
    assert!(scheduler.note_ref_updates("b", 4).unwrap().is_empty());
    let effects = scheduler.note_ref_updates("a", 1).unwrap();
    assert_eq!(effects.len(), 4, "threshold crossed for repo a");

    // The count reset: the next small update does not re-trigger.
    assert!(scheduler.note_ref_updates("a", 1).unwrap().is_empty());
    assert_eq!(sink.enqueued.lock().unwrap().len(), 1);
}

/// A sink that fails a configurable number of times.
struct FlakySink {
    failures_left: AtomicUsize,
    inner: RecordingSink,
}

impl MaintenanceSink for FlakySink {
    fn enqueue(&self, repo_id: &str, effects: &[EffectDef]) -> git_backend::Result<()> {
        if self
            .failures_left
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok()
        {
            return Err(git_backend::Error::RefStore("queue outage".to_owned()));
        }
        self.inner.enqueue(repo_id, effects)
    }
}

#[test]
fn a_sink_failure_delays_the_trigger_rather_than_losing_it() {
    let sink = std::sync::Arc::new(FlakySink {
        failures_left: AtomicUsize::new(1),
        inner: RecordingSink::default(),
    });
    let scheduler = Scheduler::new(
        Thresholds {
            maintenance: 3,
            reachability: 3,
        },
        sink.clone(),
    );

    // Crossing the threshold during the outage errors — but restores the
    // count, so the very next update re-triggers.
    let _outage = scheduler.note_ref_updates("a", 3).unwrap_err();
    let effects = scheduler.note_ref_updates("a", 1).unwrap();
    assert_eq!(effects.len(), 4);
    assert_eq!(sink.inner.enqueued.lock().unwrap().len(), 1);
}
