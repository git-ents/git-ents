//! [`InMemoryRegistry`]: an in-process [`PackRegistry`], used by tests and
//! by the conformance instantiation
//! (`crates/odb-tigris/tests/conformance.rs`).

use std::sync::{Mutex, MutexGuard, PoisonError};

use git_backend::Result;

use super::{PackId, PackRecord, PackRegistry};

/// A [`PackRegistry`] held entirely in memory, scoped to one process.
#[derive(Default)]
pub struct InMemoryRegistry {
    records: Mutex<Vec<PackRecord>>,
}

impl InMemoryRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl PackRegistry for InMemoryRegistry {
    fn record(&self, record: PackRecord) -> Result<()> {
        lock(&self.records).push(record);
        Ok(())
    }

    fn list(&self, repo_id: &str) -> Result<Vec<PackRecord>> {
        Ok(lock(&self.records)
            .iter()
            .filter(|record| record.repo_id == repo_id)
            .cloned()
            .collect())
    }

    fn delete(&self, repo_id: &str, id: &PackId) -> Result<()> {
        lock(&self.records).retain(|record| !(record.repo_id == repo_id && &record.id == id));
        Ok(())
    }
}

/// Lock `mutex`, recovering the guard from a poisoned lock rather than
/// panicking, mirroring `odb-files`'s own quarantine-map lock helper.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
