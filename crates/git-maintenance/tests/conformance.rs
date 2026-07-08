//! Closing WS2's collector seam: the causal-collection-safety property
//! (`docs/scale-out.adoc`, correctness rule 1) instantiated against
//! collectors that actually collect — [`TigrisCollector`] (grace-based,
//! including the staging-timeout boundary the suite's own docs assign to
//! the backend's instantiation) and [`FilesCollector`].

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test assertions, not application code"
)]

mod util;

use std::time::Duration;

use backend_conformance::Collector as _;
use git_backend::{ObjectStore as _, PackStream};
use git_maintenance::collector::{FilesCollector, TigrisCollector};
use git_store::test_support::{commit_all, head, repo};
use odb_tigris::OdbTigris;
use odb_tigris::registry::memory::InMemoryRegistry;
use odb_tigris::transport::fs::FsTransport;

/// The causal-collection-safety property against a Tigris store whose
/// collector really collects: `collector.collect()` runs a full expire +
/// mark-and-sweep pass while the fixture pack is staged — a collector
/// that reaped or unregistered a staged object fails the property's
/// promote/read assertions.
#[test]
fn causal_collection_safety_holds_under_a_real_tigris_collector() {
    let scratch = repo();
    let bucket = tempfile::tempdir().unwrap();
    let transport = FsTransport::open(bucket.path()).unwrap();
    let registry = InMemoryRegistry::new();
    // A generous grace window: the property itself never sleeps, so a
    // staging session inside it is always in-window — the boundary is
    // exercised separately below.
    let store =
        OdbTigris::new(&transport, &registry, "repo").with_staging_grace(Duration::from_secs(300));
    let refs = refstore_files::FilesRefStore::open(scratch.path()).unwrap();
    let collector = TigrisCollector::new("repo", &refs, &store, &transport, &registry);
    assert_eq!(collector.staging_grace(), Some(Duration::from_secs(300)));

    backend_conformance::causal_collection_safety(&store, &collector);
}

/// The staging-timeout boundary (rule 1: "a staging session that cannot
/// complete within the grace window aborts rather than becoming
/// collectible mid-flight; the suite tests the boundary"): a session held
/// past its grace window is *aborted* — its promote fails and its objects
/// stay invisible — never half-collected under a committed ref.
#[test]
fn a_staging_session_past_its_grace_window_aborts_rather_than_promotes() {
    let scratch = repo();
    let work = repo();
    let bucket = tempfile::tempdir().unwrap();
    let transport = FsTransport::open(bucket.path()).unwrap();
    let registry = InMemoryRegistry::new();
    let grace = Duration::from_millis(200);
    let store = OdbTigris::new(&transport, &registry, "repo").with_staging_grace(grace);
    let refs = refstore_files::FilesRefStore::open(scratch.path()).unwrap();
    let collector = TigrisCollector::new("repo", &refs, &store, &transport, &registry);
    assert_eq!(collector.staging_grace(), Some(grace));

    let blob = util::hash_blob(work.path(), "too-slow");
    let quarantine = store
        .stage_pack(PackStream::new(std::io::Cursor::new(util::pack_of(
            work.path(),
            &[&blob],
        ))))
        .unwrap();

    // Hold the session open past its deadline, then run a collection
    // pass — the grace-based cruft arm reaps the expired quarantine.
    std::thread::sleep(grace + Duration::from_millis(300));
    collector.collect();

    // The boundary: the session aborts. Promote must fail — succeeding
    // here would mean a session became collectible mid-flight and then
    // committed anyway — and the staged object must remain invisible.
    assert!(
        store.promote(quarantine).is_err(),
        "a staging session past its grace window must abort, not promote"
    );
    assert!(!store.contains(util::oid(&blob)).unwrap());
}

/// The same boundary without a collection pass: expiry is a property of
/// the session's own deadline, not of whether a collector happened to run
/// first — promote-after-deadline aborts either way.
#[test]
fn promote_past_the_grace_window_aborts_even_without_a_collection_pass() {
    let work = repo();
    let bucket = tempfile::tempdir().unwrap();
    let transport = FsTransport::open(bucket.path()).unwrap();
    let registry = InMemoryRegistry::new();
    let grace = Duration::from_millis(200);
    let store = OdbTigris::new(&transport, &registry, "repo").with_staging_grace(grace);

    let blob = util::hash_blob(work.path(), "also-too-slow");
    let quarantine = store
        .stage_pack(PackStream::new(std::io::Cursor::new(util::pack_of(
            work.path(),
            &[&blob],
        ))))
        .unwrap();
    std::thread::sleep(grace + Duration::from_millis(300));

    assert!(store.promote(quarantine).is_err());
    assert!(!store.contains(util::oid(&blob)).unwrap());
}

/// The causal-collection-safety property against the files backend with a
/// collector that really collects ([`git_maintenance::gc::collect_files`]).
/// No grace window: the local backend bounds staging by promotion alone.
#[test]
fn causal_collection_safety_holds_under_a_real_files_collector() {
    let dest = repo();
    // Give the collector's mark something real: a committed, reachable
    // history in the same repository the property stages into.
    util::use_main_branch(dest.path());
    std::fs::write(dest.path().join("file"), "reachable").unwrap();
    commit_all(dest.path(), "reachable");
    let reachable = head(dest.path());

    let store = odb_files::OdbFiles::open(dest.path()).unwrap();
    let collector = FilesCollector::new(dest.path());
    assert_eq!(collector.staging_grace(), None);

    backend_conformance::causal_collection_safety(&store, &collector);

    // And the pass was a real one: the repo's reachable history survived.
    let object = store.read(util::oid(&reachable)).unwrap();
    assert_eq!(object.kind, gix_object::Kind::Commit);
}
