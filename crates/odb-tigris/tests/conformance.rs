//! This crate's instantiation of the shared backend conformance suite
//! (`docs/scale-out.adoc`, WS2): every `ObjectStore` property run against
//! `OdbTigris` over its no-network stand-ins
//! ([`FsTransport`](odb_tigris::transport::fs::FsTransport) +
//! [`InMemoryRegistry`](odb_tigris::registry::memory::InMemoryRegistry)), so
//! this test needs no bucket and no Postgres to run.

#![allow(clippy::expect_used, reason = "test harness, not application code")]

use backend_conformance::NoopCollector;
use git_backend::{Object, ObjectStore, PackStream, QuarantineId, Result};
use gix_hash::ObjectId;
use odb_tigris::OdbTigris;
use odb_tigris::registry::memory::InMemoryRegistry;
use odb_tigris::transport::fs::FsTransport;

/// Bundles an `OdbTigris` over `FsTransport` with the tempdir its bucket
/// root lives under, so the directory outlives the store.
struct WithBucketDir {
    store: OdbTigris<FsTransport, InMemoryRegistry>,
    _dir: tempfile::TempDir,
}

impl WithBucketDir {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let transport = FsTransport::open(dir.path().join("bucket")).expect("open transport");
        let store = OdbTigris::new(transport, InMemoryRegistry::new(), "conformance-repo");
        Self { store, _dir: dir }
    }
}

impl ObjectStore for WithBucketDir {
    fn read(&self, id: ObjectId) -> Result<Object> {
        self.store.read(id)
    }

    fn contains(&self, id: ObjectId) -> Result<bool> {
        self.store.contains(id)
    }

    fn stage_pack(&self, pack: PackStream) -> Result<QuarantineId> {
        self.store.stage_pack(pack)
    }

    fn promote(&self, q: QuarantineId) -> Result<()> {
        self.store.promote(q)
    }
}

#[test]
fn conforms_to_object_store_properties() {
    backend_conformance::object_store_properties(WithBucketDir::new, &NoopCollector);
}
