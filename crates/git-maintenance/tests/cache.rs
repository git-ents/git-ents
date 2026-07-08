//! Cache maintenance (`docs/scale-out.adoc`, rule 4): TTL eviction
//! (expired evicted, fresh kept, registry row gone) and consolidation
//! atomicity (per-key refs deleted + consolidated ref written in one
//! transaction; reads resolve every key before, during simulated failure,
//! and after).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions, not application code"
)]

mod util;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use git_backend::{ObjectStore as _, RefName, RefStore as _, cache_ns};
use git_maintenance::{cache, gc};
use git_store::test_support::{commit_all, head, repo};
use odb_tigris::OdbTigris;
use odb_tigris::registry::PackRegistry as _;
use odb_tigris::registry::memory::InMemoryRegistry;
use odb_tigris::transport::fs::FsTransport;

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// TTL eviction end to end, including rule 4's "eviction = ref deletion +
/// registry delete": the expired ref goes, the fresh one stays, and the
/// expired entry's own cache pack (rule 5: cache objects get their own
/// packs) leaves the registry on the following collect — a registry
/// delete, never repack surgery.
#[test]
fn ttl_evicts_expired_cache_refs_and_their_registry_rows() {
    let work = repo();
    util::use_main_branch(work.path());
    std::fs::write(work.path().join("file"), "durable").unwrap();
    commit_all(work.path(), "durable");
    let durable = head(work.path());

    let bucket = tempfile::tempdir().unwrap();
    let transport = FsTransport::open(bucket.path()).unwrap();
    let registry = InMemoryRegistry::new();
    let store = OdbTigris::new(&transport, &registry, "repo");
    util::stage_and_promote(&store, util::pack_for(work.path(), &durable));

    // Two cache entries, each in its own pack (rule 5), each behind its
    // own per-key ref.
    let old_blob = util::hash_blob(work.path(), "old-entry");
    let fresh_blob = util::hash_blob(work.path(), "fresh-entry");
    util::stage_and_promote(&store, util::pack_of(work.path(), &[&old_blob]));
    util::stage_and_promote(&store, util::pack_of(work.path(), &[&fresh_blob]));

    let refs = refstore_files::FilesRefStore::open(work.path()).unwrap();

    // Reflog timestamps have one-second granularity, so age the entries
    // apart with a real gap wider than the TTL: `old` is written, the TTL
    // elapses, then `fresh` is written just before eviction runs.
    let ttl = Duration::from_secs(1);
    util::set_ref(&refs, "refs/cache/sccache/old", util::oid(&old_blob));
    std::thread::sleep(Duration::from_millis(2200));
    // A fresh store handle: gitoxide snapshots the committer signature
    // (and its timestamp) per repository open, so the second write needs
    // its own open to get a current reflog time.
    let refs = refstore_files::FilesRefStore::open(work.path()).unwrap();
    util::set_ref(&refs, "refs/cache/sccache/fresh", util::oid(&fresh_blob));

    let evicted = cache::evict_expired(&refs, ttl, now_secs()).unwrap();
    assert_eq!(
        evicted,
        vec![RefName::new("refs/cache/sccache/old")],
        "exactly the expired ref is evicted"
    );
    assert!(
        refs.get(&RefName::new("refs/cache/sccache/old"))
            .unwrap()
            .is_none(),
        "expired ref deleted"
    );
    assert!(
        refs.get(&RefName::new("refs/cache/sccache/fresh"))
            .unwrap()
            .is_some(),
        "fresh ref kept"
    );

    // The registry-delete half: after eviction, collect finds the old
    // entry's cache pack fully unreachable and deletes it whole.
    let before = registry.list("repo").unwrap().len();
    let outcome = gc::collect("repo", &refs, &store, &transport, &registry).unwrap();
    assert_eq!(outcome.deleted_packs, 1, "the evicted entry's own pack");
    assert_eq!(outcome.rewritten_packs, 0, "never repack surgery for cache");
    assert_eq!(registry.list("repo").unwrap().len(), before - 1);
    assert!(!store.contains(util::oid(&old_blob)).unwrap());
    assert!(store.contains(util::oid(&fresh_blob)).unwrap());
}

/// Consolidation atomicity over the files backend: reads resolve every
/// key before the transaction, after a simulated failure between
/// staging/promotion and the ref commit, and after the commit — and the
/// commit itself deletes every per-key ref and publishes the consolidated
/// tree in one all-or-nothing transaction.
#[test]
fn consolidation_is_atomic_and_reads_resolve_every_key_throughout() {
    let work = repo();
    util::use_main_branch(work.path());
    std::fs::write(work.path().join("file"), "seed").unwrap();
    commit_all(work.path(), "seed");

    let refs = refstore_files::FilesRefStore::open(work.path()).unwrap();
    let objects = odb_files::OdbFiles::open(work.path()).unwrap();

    let entries = [
        ("aa/bb", "value-1"),
        ("cc", "value-2"),
        ("dd/ee/ff", "value-3"),
    ];
    for (key, value) in entries {
        let blob = util::hash_blob(work.path(), value);
        util::set_ref(
            &refs,
            &format!("refs/cache/sccache/{key}"),
            util::oid(&blob),
        );
    }
    let resolve_all = |label: &str| {
        for (key, value) in entries {
            let oid = cache_ns::resolve(&refs, &objects, "sccache", key)
                .unwrap()
                .unwrap_or_else(|| panic!("{label}: key {key} must resolve"));
            assert_eq!(
                objects.read(oid).unwrap().data,
                value.as_bytes(),
                "{label}: {key}"
            );
        }
    };

    // Before.
    resolve_all("before consolidation");

    // During simulated failure: the plan is prepared — tree objects
    // staged and promoted — but the process "dies" before the ref
    // transaction. Every key still resolves through its per-key ref.
    let plan = cache::prepare_consolidation("sccache", &refs, &objects)
        .unwrap()
        .expect("three per-key refs to consolidate");
    assert_eq!(plan.keys, 3);
    resolve_all("after prepare, before commit (simulated failure window)");

    // Commit: one atomic multi-ref transaction.
    assert!(cache::commit_consolidation(&refs, &plan).unwrap());

    // After: the consolidated ref exists, every per-key ref is gone, and
    // every key still resolves — now through the tree.
    let consolidated = refs
        .get(&cache_ns::consolidated_ref("sccache"))
        .unwrap()
        .expect("consolidated ref");
    assert_eq!(consolidated, plan.tree);
    let leftover: Vec<_> = refs
        .iter_prefix(&cache_ns::per_key_prefix("sccache"))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(
        leftover.is_empty(),
        "every per-key ref deleted: {leftover:?}"
    );
    resolve_all("after consolidation");

    // A later write plus a second consolidation merges into the existing
    // tree without losing already-consolidated keys.
    let blob = util::hash_blob(work.path(), "value-4");
    util::set_ref(&refs, "refs/cache/sccache/gg", util::oid(&blob));
    assert_eq!(cache::consolidate("sccache", &refs, &objects).unwrap(), 1);
    resolve_all("after second consolidation");
    let gg = cache_ns::resolve(&refs, &objects, "sccache", "gg")
        .unwrap()
        .expect("gg resolves via the merged tree");
    assert_eq!(objects.read(gg).unwrap().data, b"value-4");
}

/// All-or-nothing under a racing writer: if any per-key ref moves between
/// prepare and commit, the whole transaction is rejected — the
/// consolidated ref is not written and no per-key ref is deleted.
#[test]
fn a_racing_cache_write_rejects_the_whole_consolidation_transaction() {
    let work = repo();
    util::use_main_branch(work.path());
    std::fs::write(work.path().join("file"), "seed").unwrap();
    commit_all(work.path(), "seed");

    let refs = refstore_files::FilesRefStore::open(work.path()).unwrap();
    let objects = odb_files::OdbFiles::open(work.path()).unwrap();

    let blob_a = util::hash_blob(work.path(), "a");
    let blob_b = util::hash_blob(work.path(), "b");
    util::set_ref(&refs, "refs/cache/sccache/aa", util::oid(&blob_a));
    util::set_ref(&refs, "refs/cache/sccache/bb", util::oid(&blob_b));

    let plan = cache::prepare_consolidation("sccache", &refs, &objects)
        .unwrap()
        .expect("two per-key refs to consolidate");

    // Race: another writer replaces one per-key ref before the commit.
    let racer = util::hash_blob(work.path(), "racer");
    util::set_ref(&refs, "refs/cache/sccache/aa", util::oid(&racer));

    assert!(
        !cache::commit_consolidation(&refs, &plan).unwrap(),
        "a moved per-key ref must reject the batch"
    );
    // Nothing changed: no consolidated ref, both per-key refs intact.
    assert!(
        refs.get(&cache_ns::consolidated_ref("sccache"))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        refs.get(&RefName::new("refs/cache/sccache/aa")).unwrap(),
        Some(util::oid(&racer))
    );
    assert_eq!(
        refs.get(&RefName::new("refs/cache/sccache/bb")).unwrap(),
        Some(util::oid(&blob_b))
    );
}
