//! CAS conformance suite for [`gix_ref_store::LooseRefStore`].
//!
//! This is the Phase 1 -> 2 gate from `docs/development-plan.adoc`:
//! "`gix-ref-store` passes a CAS conformance suite (concurrent writers,
//! crash injection)." Both properties below exercise gitoxide's own
//! on-disk lock file, not an in-process mutex standing in for it: each
//! "writer" opens its own [`LooseRefStore`] (its own `gix::Repository`
//! handle) against the same on-disk path, the way independent OS
//! processes would.
//!
//! Strategy: rstest table-driven for the fixed-shape crash-injection
//! scenario (a handful of named cases, not an unbounded input space);
//! a hand-rolled multi-thread race for concurrent writers, since the
//! property under test — exactly one of N racing CAS transactions wins,
//! observed from independent store handles — is about thread
//! interleaving, which proptest's shrinking model has nothing to offer
//! for. `@relation(..., role=Verifies)` is on each test.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "assertion helpers for a conformance suite, not application code"
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use gix_hash::ObjectId;
use gix_ref_store::{Expected, LooseRefStore, RefEdit, RefStore, RefStoreRead, TxOutcome};

/// A fresh bare repository. Bare so `dir.path()` *is* the git directory —
/// no `.git` subdirectory indirection to get wrong when a test computes a
/// ref's on-disk path directly, as the crash-injection cases below do.
fn init_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    gix::init_bare(dir.path()).expect("gix init_bare");
    dir
}

fn refname(s: &str) -> gix::refs::FullName {
    s.try_into().expect("valid refname")
}

fn oid(byte: u8) -> ObjectId {
    ObjectId::from_bytes_or_panic(&[byte; 20])
}

/// N independent store handles race a `MustNotExist` CAS create on the
/// *same* ref, each proposing a different oid. Exactly one must win; every
/// other transaction must observe the ref as already existing and report
/// `Rejected`, never silently overwrite the winner, and never both "win".
// @relation(arch.refstore-read-cas-split, arch.loose-cas-discipline, scope=function, role=Verifies)
#[test]
fn concurrent_writers_exactly_one_cas_wins() {
    let dir = init_repo();
    let name = refname("refs/meta/race");
    let writers = 8u8;

    let applied = Arc::new(AtomicUsize::new(0));
    let handles: Vec<_> = (0..writers)
        .map(|i| {
            let path = dir.path().to_path_buf();
            let name = name.clone();
            let applied = Arc::clone(&applied);
            std::thread::spawn(move || {
                // Each thread opens its own store handle against the same
                // on-disk repository, standing in for independent
                // processes contending the same loose ref file.
                let store = LooseRefStore::open(&path).expect("open");
                let outcome = store
                    .transaction(&[RefEdit {
                        name: name.clone(),
                        expected: Expected::MustNotExist,
                        new: Some(oid(i)),
                    }])
                    .expect("transaction must not error under contention, only reject");
                if outcome == TxOutcome::Applied {
                    applied.fetch_add(1, Ordering::SeqCst);
                }
                outcome
            })
        })
        .collect();

    let outcomes: Vec<TxOutcome> = handles
        .into_iter()
        .map(|h| h.join().expect("thread"))
        .collect();

    let applied_count = outcomes
        .iter()
        .filter(|o| **o == TxOutcome::Applied)
        .count();
    assert_eq!(
        applied_count, 1,
        "exactly one of {writers} racing CAS creates must apply; got {applied_count}: {outcomes:?}"
    );
    let rejected_count = outcomes
        .iter()
        .filter(|o| matches!(o, TxOutcome::Rejected { .. }))
        .count();
    assert_eq!(
        rejected_count,
        (writers - 1) as usize,
        "every non-winning transaction must be a clean Rejected, not an error or a second Applied"
    );

    // The ref must hold exactly one of the proposed values, not a torn
    // write and not a value nobody proposed.
    let store = LooseRefStore::open(dir.path()).expect("open");
    let landed = store.get(name.as_ref()).expect("get").expect("ref exists");
    assert!(
        (0..writers).map(oid).any(|candidate| candidate == landed),
        "the ref must hold exactly one racing writer's proposed oid"
    );
}

/// Concurrent writers targeting *different* refs must not falsely
/// serialize into contention with one another: independent refs are
/// independent compare-and-swap units.
// @relation(arch.refstore-read-cas-split, scope=function, role=Verifies)
#[test]
fn concurrent_writers_on_distinct_refs_all_apply() {
    let dir = init_repo();
    let writers = 8u8;

    let handles: Vec<_> = (0..writers)
        .map(|i| {
            let path = dir.path().to_path_buf();
            std::thread::spawn(move || {
                let store = LooseRefStore::open(&path).expect("open");
                store
                    .transaction(&[RefEdit {
                        name: refname(&format!("refs/meta/independent-{i}")),
                        expected: Expected::MustNotExist,
                        new: Some(oid(i)),
                    }])
                    .expect("transaction")
            })
        })
        .collect();

    for (i, handle) in handles.into_iter().enumerate() {
        let outcome = handle.join().expect("thread");
        assert_eq!(
            outcome,
            TxOutcome::Applied,
            "writer {i} on its own ref must not be blocked by unrelated concurrent writers"
        );
    }

    let store = LooseRefStore::open(dir.path()).expect("open");
    for i in 0..writers {
        assert_eq!(
            store
                .get(refname(&format!("refs/meta/independent-{i}")).as_ref())
                .expect("get"),
            Some(oid(i))
        );
    }
}

/// Simulates the on-disk artifact a writer crashing mid-transaction
/// leaves behind: a `.lock` file next to the ref, created but never
/// cleaned up because the process died holding it. A `LooseRefStore` must
/// neither corrupt the ref's last known-good value nor silently apply a
/// transaction while that lock stands; it must fail the contending
/// transaction cleanly, and a fresh transaction must succeed once the
/// stale lock is cleared, as a real recovery path (fsck / restart) would
/// clear it.
// @relation(arch.loose-cas-discipline, scope=function, role=Verifies)
#[rstest::rstest]
#[case::branch_ref("refs/heads/crash-test")]
#[case::meta_ref("refs/meta/crash-test")]
fn crash_injection_stale_lock_fails_safe_and_recovers(#[case] ref_name: &str) {
    let dir = init_repo();
    let name = refname(ref_name);
    let good = oid(0xAA);
    let attempted = oid(0xBB);

    let store = LooseRefStore::open(dir.path()).expect("open");
    let outcome = store
        .transaction(&[RefEdit {
            name: name.clone(),
            expected: Expected::MustNotExist,
            new: Some(good),
        }])
        .expect("baseline transaction");
    assert_eq!(outcome, TxOutcome::Applied);

    // Inject the artifact a crash mid-write leaves: an orphaned lock file
    // next to the loose ref, never cleaned up because nothing removed it.
    let lock_path = dir.path().join(format!("{ref_name}.lock"));
    std::fs::create_dir_all(lock_path.parent().expect("lock has a parent")).expect("mkdir -p");
    std::fs::write(&lock_path, b"orphaned by a simulated crash\n").expect("write stale lock");

    // A contending transaction must fail safely — not hang forever, not
    // silently overwrite the ref — while the stale lock stands.
    let result = store.transaction(&[RefEdit {
        name: name.clone(),
        expected: Expected::MustExistAndMatch(good),
        new: Some(attempted),
    }]);
    assert!(
        result.is_err(),
        "a transaction contending a stale lock must fail, not silently succeed or hang: {result:?}"
    );

    // The ref must be exactly as it was — no torn or partial write from
    // the failed attempt.
    assert_eq!(
        store
            .get(name.as_ref())
            .expect("get after failed transaction"),
        Some(good),
        "a failed transaction under a stale lock must not have changed the ref's value"
    );

    // Recovery: once the stale lock is cleared (as a restart or an fsck
    // pass would clear it), a fresh transaction must succeed normally.
    std::fs::remove_file(&lock_path).expect("clear the stale lock");
    let recovered = store
        .transaction(&[RefEdit {
            name: name.clone(),
            expected: Expected::MustExistAndMatch(good),
            new: Some(attempted),
        }])
        .expect("transaction after lock clears");
    assert_eq!(recovered, TxOutcome::Applied);
    assert_eq!(
        store.get(name.as_ref()).expect("get after recovery"),
        Some(attempted)
    );
}

/// The same exactly-one-wins property as
/// [`concurrent_writers_exactly_one_cas_wins`], but racing a
/// `MustExistAndMatch` update against an already-existing ref rather than
/// a `MustNotExist` create — the pattern `gate.fast-forward` and
/// `gate.atomic-cas` actually describe: a meta-ref advances from a known
/// old tip, not from nothing.
// @relation(gate.atomic-cas, arch.loose-cas-discipline, scope=function, role=Verifies)
#[test]
fn concurrent_writers_exactly_one_cas_update_wins() {
    let dir = init_repo();
    let name = refname("refs/meta/update-race");
    let store = LooseRefStore::open(dir.path()).expect("open");
    let base = oid(0x10);
    store
        .transaction(&[RefEdit {
            name: name.clone(),
            expected: Expected::MustNotExist,
            new: Some(base),
        }])
        .expect("baseline");

    let writers = 8u8;
    let handles: Vec<_> = (0..writers)
        .map(|i| {
            let path = dir.path().to_path_buf();
            let name = name.clone();
            std::thread::spawn(move || {
                let store = LooseRefStore::open(&path).expect("open");
                store
                    .transaction(&[RefEdit {
                        name: name.clone(),
                        expected: Expected::MustExistAndMatch(base),
                        new: Some(oid(0x20 + i)),
                    }])
                    .expect("transaction must not error under contention, only reject")
            })
        })
        .collect();
    let outcomes: Vec<TxOutcome> = handles
        .into_iter()
        .map(|h| h.join().expect("thread"))
        .collect();

    let applied_count = outcomes
        .iter()
        .filter(|o| **o == TxOutcome::Applied)
        .count();
    assert_eq!(
        applied_count, 1,
        "exactly one of {writers} racing CAS updates from the same known-good tip must apply; got {applied_count}: {outcomes:?}"
    );

    let landed = store.get(name.as_ref()).expect("get").expect("ref exists");
    assert!(
        (0..writers)
            .map(|i| oid(0x20 + i))
            .any(|candidate| candidate == landed),
        "the ref must hold exactly one racing writer's proposed oid, not the stale base value"
    );
}
