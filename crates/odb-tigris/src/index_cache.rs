//! In-memory cache of parsed `.idx` bytes, one per [`crate::OdbTigris`]
//! instance (`docs/scale-out.adoc`, WS5: "cache fetched `.idx` bytes in
//! memory per store"). Indexes are small (a few percent of pack size) and
//! reused across every `read`/`contains` call, so fetching one once per
//! pack per store lifetime — rather than per object lookup — is the whole
//! point of this module.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use git_backend::Result;
use gix_pack::index::File as IndexFile;

use crate::transport::BlobTransport;

/// A cache of parsed pack indexes, keyed by their bucket key.
#[derive(Default)]
pub struct IndexCache {
    parsed: Mutex<HashMap<String, Arc<IndexFile<Vec<u8>>>>>,
}

impl IndexCache {
    /// An empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the parsed index for `idx_key`, fetching and parsing it via
    /// `transport` on a cache miss.
    ///
    /// # Errors
    ///
    /// Returns an error if the fetch or parse fails.
    pub fn get(
        &self,
        transport: &dyn BlobTransport,
        idx_key: &str,
        object_hash: gix_hash::Kind,
    ) -> Result<Arc<IndexFile<Vec<u8>>>> {
        if let Some(cached) = lock(&self.parsed).get(idx_key) {
            return Ok(Arc::clone(cached));
        }
        let bytes = transport.get(idx_key)?;
        let parsed = IndexFile::from_data(bytes, std::path::PathBuf::from(idx_key), object_hash)
            .map_err(|error| {
                git_backend::Error::ObjectStore(format!("parsing index {idx_key}: {error}"))
            })?;
        let parsed = Arc::new(parsed);
        lock(&self.parsed).insert(idx_key.to_owned(), Arc::clone(&parsed));
        Ok(parsed)
    }

    /// Drop a cached index, e.g. because its pack was deleted from the
    /// registry. Not currently called on any path in this crate (GC is out
    /// of scope for WS5), but present so a future maintenance path doesn't
    /// need to add cache invalidation from scratch.
    pub fn invalidate(&self, idx_key: &str) {
        lock(&self.parsed).remove(idx_key);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
