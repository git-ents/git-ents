//! [`ObjectStore`] over an S3-compatible bucket (Tigris in production),
//! designed around ranged reads rather than whole-pack hydration
//! (`docs/scale-out.adoc`, "ObjectStore" / WS5).
//!
//! # Layout
//!
//! Packs and their `.idx` live under a per-repo prefix (rule 7: "namespace
//! per repo" — no cross-tenant dedup, so every key this crate writes or
//! reads is scoped under `{repo_id}/...`):
//!
//! - `{repo_id}/quarantine/{id}/pack.pack` + `.../pack.idx` — staged, not
//!   yet visible (written by [`OdbTigris::stage_pack`]).
//! - `{repo_id}/live/{id}.pack` + `{id}.idx` — promoted, registered, visible
//!   to `read`/`contains` (written by [`OdbTigris::promote`]).
//!
//! midx (multi-pack-index) support is not implemented: nothing here
//! prevents adding one alongside the per-pack indexes later
//! (`docs/scale-out.adoc`'s ObjectStore row mentions it as "when
//! available"), but with no multi-index yet, `read`/`contains` scan every
//! registered pack's own `.idx` — correct, and cheap enough for the pack
//! counts this store is expected to carry before WS6 lands.
//!
//! # Never a bucket listing
//!
//! [`OdbTigris`] never calls anything resembling a bucket "list objects"
//! operation — the [`transport::BlobTransport`] trait doesn't even expose
//! one. Every key this store touches comes from either a
//! [`registry::PackRegistry`] record or a quarantine id it minted itself
//! (`docs/scale-out.adoc`, "Reachability": "nothing may traverse Tigris
//! object-by-object").
//!
//! # Q2: promotion visibility
//!
//! See [`transport`]'s module doc: `promote` assumes the bucket offers
//! read-after-write consistency for a key it just copied.

pub mod decode;
pub mod index_cache;
pub mod pack_writer;
pub mod registry;
pub mod transport;

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

pub use git_backend::{Error, Result};
use git_backend::{Object, ObjectStore, PackStream, QuarantineId};
use gix_hash::ObjectId;
use gix_pack::index::File as IndexFile;

use crate::decode::RefDeltaResolver;
use crate::index_cache::IndexCache;
use crate::registry::{PackId, PackRecord, PackRegistry};
use crate::transport::BlobTransport;

/// A registered pack paired with its parsed index, as gathered by
/// [`OdbTigris::indexes`].
type RecordAndIndex = (PackRecord, std::sync::Arc<IndexFile<Vec<u8>>>);

/// A pack awaiting promotion: the quarantine keys [`OdbTigris::stage_pack`]
/// uploaded to, kept around so [`OdbTigris::promote`] knows what to copy.
struct Quarantine {
    pack_key: String,
    idx_key: String,
    object_count: u64,
}

/// [`ObjectStore`] over an S3-compatible bucket, generic over its blob
/// transport and pack registry so tests can run with
/// [`transport::fs::FsTransport`] + [`registry::memory::InMemoryRegistry`]
/// and production wires up [`transport::s3::S3Transport`] plus a
/// Postgres-backed registry (see `refstore-postgres`).
pub struct OdbTigris<T, R> {
    transport: T,
    registry: R,
    repo_id: String,
    hash_kind: gix_hash::Kind,
    index_cache: IndexCache,
    quarantines: Mutex<HashMap<QuarantineId, Quarantine>>,
}

impl<T, R> OdbTigris<T, R>
where
    T: BlobTransport,
    R: PackRegistry,
{
    /// Open a store scoped to `repo_id`, over `transport` and `registry`.
    /// Object hashes are always SHA-1, matching every other backend in this
    /// workspace.
    pub fn new(transport: T, registry: R, repo_id: impl Into<String>) -> Self {
        Self {
            transport,
            registry,
            repo_id: repo_id.into(),
            hash_kind: gix_hash::Kind::Sha1,
            index_cache: IndexCache::new(),
            quarantines: Mutex::new(HashMap::new()),
        }
    }

    fn quarantine_pack_key(&self, id: &str) -> String {
        format!("{}/quarantine/{id}/pack.pack", self.repo_id)
    }

    fn quarantine_idx_key(&self, id: &str) -> String {
        format!("{}/quarantine/{id}/pack.idx", self.repo_id)
    }

    fn live_pack_key(&self, id: &str) -> String {
        format!("{}/live/{id}.pack", self.repo_id)
    }

    fn live_idx_key(&self, id: &str) -> String {
        format!("{}/live/{id}.idx", self.repo_id)
    }

    /// Every registered pack's parsed index, fetched (and cached) on
    /// demand. Iterated in full on every `read`/`contains` since there is
    /// no multi-pack-index yet — see this crate's module doc.
    fn indexes(&self) -> Result<Vec<RecordAndIndex>> {
        self.registry
            .list(&self.repo_id)?
            .into_iter()
            .map(|record| {
                let idx = self
                    .index_cache
                    .get(&self.transport, &record.idx_key, self.hash_kind)?;
                Ok((record, idx))
            })
            .collect()
    }
}

impl<T, R> ObjectStore for OdbTigris<T, R>
where
    T: BlobTransport,
    R: PackRegistry,
{
    fn read(&self, id: ObjectId) -> Result<Object> {
        for (record, idx) in self.indexes()? {
            let Some(entry_index) = idx.lookup(id) else {
                continue;
            };
            let offset = idx.pack_offset_at_index(entry_index);
            let resolver = IndexRefDeltaResolver { idx: &idx };
            let (kind, data) = decode::resolve(
                &self.transport,
                &record.pack_key,
                offset,
                self.hash_kind.len_in_bytes(),
                &resolver,
            )?;
            return Ok(Object { kind, data });
        }
        Err(Error::ObjectStore(format!(
            "object {id} not found in any registered pack for repo {}",
            self.repo_id
        )))
    }

    fn contains(&self, id: ObjectId) -> Result<bool> {
        for (_record, idx) in self.indexes()? {
            if idx.lookup(id).is_some() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn stage_pack(&self, pack: PackStream) -> Result<QuarantineId> {
        let id = uuid::Uuid::new_v4().to_string();
        let scratch = tempfile::tempdir()?;

        let mut reader = std::io::BufReader::new(pack);
        let outcome = gix_pack::Bundle::write_to_directory(
            &mut reader,
            Some(scratch.path()),
            &mut gix_features::progress::Discard,
            &std::sync::atomic::AtomicBool::new(false),
            None::<NoThinBaseLookup>,
            gix_pack::bundle::write::Options {
                object_hash: self.hash_kind,
                ..Default::default()
            },
        )
        .map_err(|error| Error::ObjectStore(error.to_string()))?;

        let data_path = outcome
            .data_path
            .ok_or_else(|| Error::ObjectStore("pack write produced no data file".to_owned()))?;
        let index_path = outcome
            .index_path
            .ok_or_else(|| Error::ObjectStore("pack write produced no index file".to_owned()))?;
        let pack_bytes = std::fs::read(&data_path)?;
        let idx_bytes = std::fs::read(&index_path)?;

        self.transport
            .put(&self.quarantine_pack_key(&id), pack_bytes)?;
        self.transport
            .put(&self.quarantine_idx_key(&id), idx_bytes)?;

        lock(&self.quarantines).insert(
            QuarantineId::new(id.clone()),
            Quarantine {
                pack_key: self.quarantine_pack_key(&id),
                idx_key: self.quarantine_idx_key(&id),
                object_count: u64::from(outcome.index.num_objects),
            },
        );
        Ok(QuarantineId::new(id))
    }

    fn promote(&self, q: QuarantineId) -> Result<()> {
        let quarantine = lock(&self.quarantines)
            .remove(&q)
            .ok_or_else(|| Error::ObjectStore(format!("unknown quarantine {q}")))?;

        let live_pack_key = self.live_pack_key(q.as_str());
        let live_idx_key = self.live_idx_key(q.as_str());

        // Q2: the copy below must be durably visible to a subsequent `get`/
        // `get_range` against `live_pack_key`/`live_idx_key` before we
        // return — see `transport`'s module doc. This is a fact about the
        // bucket to verify, not something this code can enforce.
        self.transport.copy(&quarantine.pack_key, &live_pack_key)?;
        self.transport.copy(&quarantine.idx_key, &live_idx_key)?;

        self.registry.record(PackRecord {
            id: PackId::new(q.as_str()),
            repo_id: self.repo_id.clone(),
            pack_key: live_pack_key,
            idx_key: live_idx_key,
            object_count: Some(quarantine.object_count),
        })?;

        // Best-effort cleanup: the registry record above is the actual
        // commit point (rule 2). Leaving these behind would waste space,
        // not correctness, so a failure here is not propagated.
        let _ignored = self.transport.delete(&quarantine.pack_key);
        let _ignored = self.transport.delete(&quarantine.idx_key);
        Ok(())
    }
}

/// Resolves `RefDelta` bases against one pack's already-fetched index — see
/// `decode`'s module doc for why a ref-delta base is always in the same
/// pack for stores built by this crate.
struct IndexRefDeltaResolver<'a> {
    idx: &'a IndexFile<Vec<u8>>,
}

impl RefDeltaResolver for IndexRefDeltaResolver<'_> {
    fn resolve(&self, base_id: &gix_hash::oid) -> Option<u64> {
        let entry_index = self.idx.lookup(base_id)?;
        Some(self.idx.pack_offset_at_index(entry_index))
    }
}

/// A `gix_object::Find` that never finds anything, satisfying
/// `Bundle::write_to_directory`'s thin-pack-base-lookup parameter without
/// pulling in a full `gix`/`gix-odb` dependency. Correct because this
/// store's `stage_pack` never receives a thin pack: every pack it indexes
/// is expected to be self-contained (the same assumption `odb-files` makes
/// by passing `None` there too).
struct NoThinBaseLookup;

impl gix_object::Find for NoThinBaseLookup {
    fn try_find<'a>(
        &self,
        _id: &gix_hash::oid,
        _buffer: &'a mut Vec<u8>,
    ) -> std::result::Result<Option<gix_object::Data<'a>>, gix_object::find::Error> {
        Ok(None)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
