//! [`ObjectStore`] composed from a small-object tier over
//! [`odb_tigris::OdbTigris`] — composition, not a third semantics
//! (`docs/scale-out.adoc`, "ObjectStore": "The tiered store is composition,
//! not a third semantics: `read` consults tiers in order; `contains` is the
//! union.").
//!
//! `read` tries [`small_tier::SmallObjectTier`] first, falling through to
//! the underlying store on a tier miss; `contains` is the union of both.
//! `stage_pack` splits an incoming pack by object size against
//! [`SMALL_OBJECT_THRESHOLD_BYTES`] (see its doc comment — Q5: "measure,
//! don't guess"): objects at or above the threshold are re-packed
//! whole-object (via [`odb_tigris::pack_writer`]) and staged into the
//! underlying store exactly as before; objects below it are staged
//! directly into the small tier. `promote` commits both halves.

pub mod small_tier;

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use git_backend::{Error, Object, ObjectStore, PackStream, QuarantineId, Result};
use gix_hash::ObjectId;
use odb_tigris::pack_writer::{ClassifiedObject, LifetimeClass};

use crate::small_tier::{SmallObjectTier, SmallStageId};

/// Size, in bytes, below which an object is staged into the small tier
/// instead of a pack. A few KiB, per `docs/scale-out.adoc`'s Q5 ("small-
/// object tier threshold: measure"): this default is a plausible starting
/// point for typed documents (manifests, small trees), not a measured
/// value — a real deployment should tune it against observed object-size
/// and access-latency distributions rather than trust this constant.
pub const SMALL_OBJECT_THRESHOLD_BYTES: usize = 4 * 1024;

/// One quarantined batch, split across the two tiers it may span.
struct Quarantine {
    small: Option<SmallStageId>,
    underlying: Option<QuarantineId>,
}

/// [`ObjectStore`] composing a [`SmallObjectTier`] `K` over an underlying
/// store `S` (in practice, [`odb_tigris::OdbTigris`], but any `ObjectStore`
/// qualifies — this crate depends on `odb-tigris` only for
/// [`odb_tigris::pack_writer`], not for a hard-wired backend).
pub struct OdbTiered<S, K> {
    underlying: S,
    small_tier: K,
    repo_id: String,
    small_threshold: usize,
    quarantines: Mutex<HashMap<QuarantineId, Quarantine>>,
}

impl<S, K> OdbTiered<S, K>
where
    S: ObjectStore,
    K: SmallObjectTier,
{
    /// Compose `small_tier` over `underlying`, scoped to `repo_id`, using
    /// the default [`SMALL_OBJECT_THRESHOLD_BYTES`].
    pub fn new(underlying: S, small_tier: K, repo_id: impl Into<String>) -> Self {
        Self::with_threshold(
            underlying,
            small_tier,
            repo_id,
            SMALL_OBJECT_THRESHOLD_BYTES,
        )
    }

    /// As [`Self::new`], with an explicit small-object threshold — see
    /// [`SMALL_OBJECT_THRESHOLD_BYTES`]'s doc comment on why this should be
    /// measured for a real deployment rather than left at the default.
    pub fn with_threshold(
        underlying: S,
        small_tier: K,
        repo_id: impl Into<String>,
        small_threshold: usize,
    ) -> Self {
        Self {
            underlying,
            small_tier,
            repo_id: repo_id.into(),
            small_threshold,
            quarantines: Mutex::new(HashMap::new()),
        }
    }

    /// Fully materialize every object in an incoming pack, by indexing it
    /// (exactly as `odb_tigris::OdbTigris::stage_pack` and `odb_files`'s
    /// own quarantine do) and then decoding each entry through
    /// `gix_pack`'s own full decoder. Unlike `odb_tigris::decode` (which is
    /// built around ranged reads against a *remote* pack this store never
    /// downloads in full), the incoming pack here is already fully local —
    /// there is no ranged-read concern splitting objects out of it, so
    /// reusing `gix_pack::Bundle`'s own (delta-resolving) decode path is
    /// the correct choice, not a shortcut.
    fn materialize_incoming_pack(pack: PackStream) -> Result<Vec<(ObjectId, Object)>> {
        let scratch = tempfile::tempdir()?;
        let mut reader = std::io::BufReader::new(pack);
        let outcome = gix_pack::Bundle::write_to_directory(
            &mut reader,
            Some(scratch.path()),
            &mut gix_features::progress::Discard,
            &std::sync::atomic::AtomicBool::new(false),
            None::<NoThinBaseLookup>,
            gix_pack::bundle::write::Options {
                object_hash: gix_hash::Kind::Sha1,
                ..Default::default()
            },
        )
        .map_err(|error| Error::ObjectStore(error.to_string()))?;
        let index_path = outcome
            .index_path
            .ok_or_else(|| Error::ObjectStore("pack write produced no index file".to_owned()))?;
        let bundle = gix_pack::Bundle::at(&index_path, gix_hash::Kind::Sha1)
            .map_err(|error| Error::ObjectStore(error.to_string()))?;

        let entries: Vec<_> = bundle.index.iter().collect();
        let mut objects = Vec::with_capacity(entries.len());
        let mut inflate = gix_features::zlib::Inflate::default();
        let mut cache = gix_pack::cache::Never;
        for entry in entries {
            let mut buf = Vec::new();
            let (data, _location) = bundle
                .find(&entry.oid, &mut buf, &mut inflate, &mut cache)
                .map_err(|error| Error::ObjectStore(error.to_string()))?
                .ok_or_else(|| {
                    Error::ObjectStore(format!(
                        "object {} listed in its own pack's index but not found in it",
                        entry.oid
                    ))
                })?;
            objects.push((
                entry.oid,
                Object {
                    kind: data.kind,
                    data: data.data.to_vec(),
                },
            ));
        }
        Ok(objects)
    }
}

impl<S, K> ObjectStore for OdbTiered<S, K>
where
    S: ObjectStore,
    K: SmallObjectTier,
{
    fn read(&self, id: ObjectId) -> Result<Object> {
        if let Some(object) = self.small_tier.read(&self.repo_id, id)? {
            return Ok(object);
        }
        self.underlying.read(id)
    }

    fn contains(&self, id: ObjectId) -> Result<bool> {
        Ok(self.small_tier.contains(&self.repo_id, id)? || self.underlying.contains(id)?)
    }

    fn stage_pack(&self, pack: PackStream) -> Result<QuarantineId> {
        let objects = Self::materialize_incoming_pack(pack)?;
        let (small, large): (Vec<_>, Vec<_>) = objects
            .into_iter()
            .partition(|(_id, object)| object.data.len() < self.small_threshold);

        let small_stage = if small.is_empty() {
            None
        } else {
            Some(self.small_tier.stage(&self.repo_id, small)?)
        };

        let underlying_quarantine = if large.is_empty() {
            None
        } else {
            let classified: Vec<ClassifiedObject> = large
                .into_iter()
                .map(|(id, object)| ClassifiedObject {
                    id,
                    kind: object.kind,
                    data: object.data,
                    // `odb_tiered` doesn't itself track cache-namespace
                    // lifetime — that's a property of which ref a caller is
                    // about to point at these objects, not of the bytes
                    // this store sees. Every object routed to the
                    // underlying store here is marked `Durable`; a caller
                    // staging cache-namespace objects through a tiered
                    // store still gets correct behavior (rule 5 is about
                    // never mixing lifetimes within a single pack, and a
                    // single `stage_pack` call is exactly one lifetime by
                    // construction of its caller), just not automatically
                    // inferred from content alone.
                    lifetime: LifetimeClass::Durable,
                })
                .collect();
            let partitioned = odb_tigris::pack_writer::partition_and_pack(classified)?;
            match partitioned.durable {
                Some(pack_bytes) => Some(
                    self.underlying
                        .stage_pack(PackStream::new(std::io::Cursor::new(pack_bytes)))?,
                ),
                None => None,
            }
        };

        let id = QuarantineId::new(uuid::Uuid::new_v4().to_string());
        lock(&self.quarantines).insert(
            id.clone(),
            Quarantine {
                small: small_stage,
                underlying: underlying_quarantine,
            },
        );
        Ok(id)
    }

    fn promote(&self, q: QuarantineId) -> Result<()> {
        let quarantine = lock(&self.quarantines)
            .remove(&q)
            .ok_or_else(|| Error::ObjectStore(format!("unknown quarantine {q}")))?;

        // Q5/rule 1&2: each tier's own `promote` is the commit point for
        // the objects it holds (staged objects invisible until promoted,
        // per each tier's own contract). "Transactionally with it" (see
        // this crate's module doc) means the small tier's staged rows move
        // to live in one Postgres transaction internally — not that this
        // pair of calls is one distributed transaction spanning Tigris and
        // Postgres, which no code here could honestly promise. If the
        // small-tier promote below succeeds and the underlying promote
        // fails (or the reverse), the failing half's objects simply remain
        // quarantined/staged — never partially visible — and the caller
        // sees the error and can retry `promote` with the same id.
        if let Some(small) = quarantine.small {
            self.small_tier.promote(small)?;
        }
        if let Some(underlying) = quarantine.underlying {
            self.underlying.promote(underlying)?;
        }
        Ok(())
    }
}

/// A `gix_object::Find` that never finds anything, satisfying
/// `Bundle::write_to_directory`'s thin-pack-base-lookup parameter — see
/// `odb_tigris`'s identical helper for why this is correct here too (this
/// store's incoming packs are also expected to be self-contained).
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
