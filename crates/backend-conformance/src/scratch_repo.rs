//! [`WithScratchRepo`]: keeps a file-backed backend alive alongside the
//! scratch git repository its on-disk state depends on.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "fixture helper for a conformance suite, not application code"
)]

use std::path::Path;

use git_backend::{
    Object, ObjectStore, PackStream, QuarantineId, RefEdit, RefEventStream, RefIter, RefLogIter,
    RefName, RefStore, Result, TxOutcome,
};
use gix_hash::ObjectId;

use crate::FixtureOids;
use crate::support::commit_oids_into;

/// Bundles a backend with the throwaway git repository it was opened
/// against, so the repository outlives the backend: struct fields drop in
/// declaration order, so `store` (which may hold open handles into the
/// repository) is released before `_dir` deletes it.
///
/// Local file-backed backends (`refstore-files`, `odb-files`) need a real
/// repository on disk to open against; this is a `mk` closure's return
/// value in a conformance instantiation for that shape of backend. Cloud
/// backends have no such requirement and do not need this type.
pub struct WithScratchRepo<S> {
    store: S,
    _dir: tempfile::TempDir,
}

impl<S> WithScratchRepo<S> {
    /// Create a fresh scratch git repository and hand its path to `open` to
    /// build the backend, keeping the repository alive for as long as the
    /// returned value lives.
    pub fn new<E: std::fmt::Debug>(open: impl FnOnce(&Path) -> std::result::Result<S, E>) -> Self {
        let dir = git_store::test_support::repo();
        let store = open(dir.path()).expect("open backend against scratch repo");
        Self { store, _dir: dir }
    }
}

impl<S: RefStore> RefStore for WithScratchRepo<S> {
    fn get(&self, name: &RefName) -> Result<Option<ObjectId>> {
        self.store.get(name)
    }

    fn iter_prefix(&self, prefix: &RefName) -> Result<RefIter> {
        self.store.iter_prefix(prefix)
    }

    fn transaction(&self, edits: &[RefEdit]) -> Result<TxOutcome> {
        self.store.transaction(edits)
    }

    fn watch(&self, prefix: &RefName) -> Result<RefEventStream> {
        self.store.watch(prefix)
    }

    fn log(&self, name: &RefName) -> Result<RefLogIter> {
        self.store.log(name)
    }
}

impl<S> FixtureOids for WithScratchRepo<S> {
    fn fixture_oids(&self, n: usize) -> Vec<ObjectId> {
        commit_oids_into(self._dir.path(), n)
    }
}

impl<S: ObjectStore> ObjectStore for WithScratchRepo<S> {
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
