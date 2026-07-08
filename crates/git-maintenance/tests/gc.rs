//! Mark-and-sweep correctness (`docs/scale-out.adoc`, WS9): fully
//! unreachable packs are deleted, mixed packs are rewritten preserving
//! every reachable object, and staged/quarantined objects are untouched
//! (correctness rules 1 and 2).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test assertions, not application code"
)]

mod util;

use git_backend::{ObjectStore as _, PackStream};
use git_maintenance::gc;
use git_store::test_support::{commit_all, head, repo};
use odb_tigris::OdbTigris;
use odb_tigris::registry::PackRegistry as _;
use odb_tigris::registry::memory::InMemoryRegistry;
use odb_tigris::transport::{BlobTransport as _, fs::FsTransport};

#[test]
fn a_fully_unreachable_pack_is_deleted_registry_first() {
    let work = repo();
    util::use_main_branch(work.path());
    std::fs::write(work.path().join("file"), "one").unwrap();
    commit_all(work.path(), "a");
    let a = head(work.path());

    // An independent root the refs will never point at once its branch is
    // gone: its pack becomes fully unreachable.
    util::git(work.path(), &["checkout", "-q", "--orphan", "gone"]);
    std::fs::write(work.path().join("file"), "doomed").unwrap();
    commit_all(work.path(), "c");
    let c = head(work.path());
    util::git(work.path(), &["checkout", "-q", "main"]);
    util::git(work.path(), &["branch", "-q", "-D", "gone"]);

    let bucket = tempfile::tempdir().unwrap();
    let transport = FsTransport::open(bucket.path()).unwrap();
    let registry = InMemoryRegistry::new();
    let store = OdbTigris::new(&transport, &registry, "repo");
    util::stage_and_promote(&store, util::pack_for(work.path(), &a));
    util::stage_and_promote(&store, util::pack_for(work.path(), &c));
    assert_eq!(registry.list("repo").unwrap().len(), 2);

    let refs = refstore_files::FilesRefStore::open(work.path()).unwrap();
    let outcome = gc::collect("repo", &refs, &store, &transport, &registry).unwrap();

    assert_eq!(outcome.deleted_packs, 1);
    assert_eq!(outcome.rewritten_packs, 0);
    let remaining = registry.list("repo").unwrap();
    assert_eq!(remaining.len(), 1, "only the reachable pack remains");
    assert!(store.contains(util::oid(&a)).unwrap());
    assert!(
        !store.contains(util::oid(&c)).unwrap(),
        "the unreachable commit's pack must be gone from the registry"
    );
    // The blobs are gone from the bucket too, not just unregistered.
    for record in &remaining {
        assert!(transport.exists(&record.pack_key).unwrap());
    }
}

#[test]
fn a_mixed_pack_is_rewritten_preserving_reachable_objects() {
    let work = repo();
    util::use_main_branch(work.path());
    std::fs::write(work.path().join("file"), "one").unwrap();
    commit_all(work.path(), "a");
    let a = head(work.path());
    std::fs::write(work.path().join("file2"), "two").unwrap();
    commit_all(work.path(), "b");
    let b = head(work.path());

    let bucket = tempfile::tempdir().unwrap();
    let transport = FsTransport::open(bucket.path()).unwrap();
    let registry = InMemoryRegistry::new();
    let store = OdbTigris::new(&transport, &registry, "repo");
    // One pack holding both commits' closures.
    util::stage_and_promote(&store, util::pack_for(work.path(), &b));
    let original = registry.list("repo").unwrap();
    assert_eq!(original.len(), 1);
    let original = original.into_iter().next().unwrap();

    // Rewind main to `a`: `b`'s commit/tree/blob become unreachable, `a`'s
    // closure stays live — a mixed pack.
    util::git(work.path(), &["update-ref", "refs/heads/main", &a]);
    util::git(work.path(), &["reset", "-q", "--hard", &a]);

    let refs = refstore_files::FilesRefStore::open(work.path()).unwrap();
    let outcome = gc::collect("repo", &refs, &store, &transport, &registry).unwrap();

    assert_eq!(outcome.deleted_packs, 0);
    assert_eq!(outcome.rewritten_packs, 1);
    let rewritten = registry.list("repo").unwrap();
    assert_eq!(rewritten.len(), 1);
    let rewritten = rewritten.into_iter().next().unwrap();
    assert_ne!(rewritten.id, original.id, "the old pack was replaced");
    assert!(
        !transport.exists(&original.pack_key).unwrap(),
        "the old mixed pack's bytes are gone"
    );

    // Every reachable object survived the rewrite; the unreachable commit
    // did not.
    let commit_a = store.read(util::oid(&a)).unwrap();
    assert_eq!(commit_a.kind, gix_object::Kind::Commit);
    assert!(!store.contains(util::oid(&b)).unwrap());
    // a's closure is commit + tree + one blob.
    assert_eq!(rewritten.object_count, Some(3));
}

#[test]
fn staged_quarantined_objects_are_untouched_by_a_collection_pass() {
    let work = repo();
    util::use_main_branch(work.path());
    std::fs::write(work.path().join("file"), "committed").unwrap();
    commit_all(work.path(), "committed");
    let committed = head(work.path());

    let bucket = tempfile::tempdir().unwrap();
    let transport = FsTransport::open(bucket.path()).unwrap();
    let registry = InMemoryRegistry::new();
    let store = OdbTigris::new(&transport, &registry, "repo");
    util::stage_and_promote(&store, util::pack_for(work.path(), &committed));

    // Staged for an in-flight transaction, never promoted. The sweep
    // enumerates the registry only — structurally, a quarantine cannot
    // even be named by it (rule 2: "GC never scans quarantine").
    let staged_blob = util::hash_blob(work.path(), "in-flight");
    let quarantine = store
        .stage_pack(PackStream::new(std::io::Cursor::new(util::pack_of(
            work.path(),
            &[&staged_blob],
        ))))
        .unwrap();

    let refs = refstore_files::FilesRefStore::open(work.path()).unwrap();
    let outcome = gc::collect("repo", &refs, &store, &transport, &registry).unwrap();
    assert_eq!(outcome.deleted_packs, 0);

    // The in-flight transaction can still commit: promote succeeds and
    // the objects are correct — a collector that touched quarantine would
    // fail here.
    store.promote(quarantine).unwrap();
    let object = store.read(util::oid(&staged_blob)).unwrap();
    assert_eq!(object.kind, gix_object::Kind::Blob);
    assert_eq!(object.data, b"in-flight");
}
