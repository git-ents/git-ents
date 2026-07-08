//! Partitioning objects into packs that obey the pack-lifetime rule
//! (`docs/scale-out.adoc`, rule 5): "Objects with different lifetimes never
//! share a pack. Cache-namespace objects get their own packs, so eviction is
//! a registry delete, never repack surgery. Within a lifetime class, delta
//! policy is per content class: trees/manifests delta'd, binaries stored
//! raw."
//!
//! # Decision point: whole-object encoding only
//!
//! `gix_pack::data::output::Entry::from_data` — the constructor this module
//! (and `git-protocol`'s `pack::build_pack`, the precedent this mirrors)
//! uses to turn a materialized object into a pack entry — only ever
//! produces [`gix_pack::data::output::entry::Kind::Base`], i.e. a full
//! object. The `DeltaRef`/`DeltaOid` variants of that enum exist, but the
//! only public constructor that produces them,
//! `output::Entry::from_pack_entry`, *reuses* a delta already present in a
//! source pack being repacked — gix-pack exposes no API to diff two fresh
//! objects and encode a new delta from scratch. That's a real gap, not a
//! missed method: encoding a new delta requires an actual diff algorithm,
//! which is out of scope to bolt on here (`docs/scale-out.adoc`, Q6, calls
//! this out as its own risk-budgeted item).
//!
//! So [`ContentClass`] is honestly a *policy* label, not yet a behavior:
//! this module partitions by [`LifetimeClass`] (rule 5's actual correctness
//! requirement — cache and durable objects never share a pack) and records
//! each object's [`ContentClass`] purely as the documented decision point
//! for when gix-pack (or a future dependency) gains real delta-encoding.
//! Every object, regardless of class, is written whole today. Do not read
//! `ContentClass::Structural` as "this gets delta-compressed" — it does
//! not, yet.

use gix_hash::ObjectId;
use gix_object::Kind;
use gix_pack::data::Version;
use gix_pack::data::output::{Count, Entry, bytes::FromEntriesIter};

use crate::Result;

/// Which lifetime an object belongs to (`docs/scale-out.adoc`, rule 5).
/// Determines which of the two output packs an object lands in; never mixed
/// within one pack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifetimeClass {
    /// Ordinary repository objects: reachable from durable refs, never
    /// evicted by a cache TTL.
    Durable,
    /// Objects reachable only from `refs/cache/*` / `refs/meta/cache/*`
    /// (`docs/scale-out.adoc`, rule 4): evictable, reconstructible, exempt
    /// from provenance.
    Cache,
}

/// The content-class delta policy this object is a candidate for, per rule
/// 5's "within a lifetime class, delta policy is per content class" —
/// currently a documented decision point rather than an applied behavior;
/// see this module's doc comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentClass {
    /// Trees, commits, and typed manifests: eligible for delta compression
    /// once gix-pack (or a replacement) can encode one.
    Structural,
    /// Blobs at or above the raw-storage threshold: stored whole
    /// deliberately (rule 5, and `crate::decode`'s doc comment on why short
    /// delta chains matter for ranged reads).
    Binary,
}

/// Size, in bytes, at or above which a blob is classified [`ContentClass::Binary`]
/// rather than [`ContentClass::Structural`]. Chosen as a plausible default,
/// not measured; `docs/scale-out.adoc`'s Q5 applies to the tiered store's
/// small-object threshold specifically, but the same "measure, don't guess"
/// caution applies here.
pub const BINARY_THRESHOLD_BYTES: usize = 16 * 1024;

/// One object to be written into a pack by [`partition_and_pack`].
pub struct ClassifiedObject {
    /// The object's id.
    pub id: ObjectId,
    /// The object's kind.
    pub kind: Kind,
    /// The object's raw, undeltified content.
    pub data: Vec<u8>,
    /// Which lifetime this object belongs to.
    pub lifetime: LifetimeClass,
}

impl ClassifiedObject {
    /// This object's content class, derived from its kind and size against
    /// [`BINARY_THRESHOLD_BYTES`] (see [`ContentClass`]).
    #[must_use]
    pub fn content_class(&self) -> ContentClass {
        match self.kind {
            Kind::Tree | Kind::Commit | Kind::Tag => ContentClass::Structural,
            Kind::Blob if self.data.len() < BINARY_THRESHOLD_BYTES => ContentClass::Structural,
            Kind::Blob => ContentClass::Binary,
        }
    }
}

/// The result of [`partition_and_pack`]: up to two whole-object version-2
/// packs, one per [`LifetimeClass`] actually present in the input. A class
/// with no objects produces no pack at all — rule 5 forbids an empty
/// cache-namespace pack sharing anything with the durable one, but there's
/// no reason to write one when there's nothing to put in it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PartitionedPacks {
    /// The durable-class pack, if any durable objects were supplied.
    pub durable: Option<Vec<u8>>,
    /// The cache-class pack, if any cache-namespace objects were supplied.
    pub cache: Option<Vec<u8>>,
}

/// Partition `objects` by [`LifetimeClass`] and encode each non-empty
/// partition as its own version-2 pack (`docs/scale-out.adoc`, rule 5).
///
/// # Errors
///
/// Returns an error if pack encoding fails (e.g. zlib deflate failure).
pub fn partition_and_pack(objects: Vec<ClassifiedObject>) -> Result<PartitionedPacks> {
    let (durable, cache): (Vec<_>, Vec<_>) = objects
        .into_iter()
        .partition(|object| object.lifetime == LifetimeClass::Durable);
    Ok(PartitionedPacks {
        durable: pack_whole_objects(&durable)?,
        cache: pack_whole_objects(&cache)?,
    })
}

/// Re-index freshly encoded, self-contained pack bytes (as produced by
/// [`pack_whole_objects`]/[`partition_and_pack`]) into `(pack_bytes,
/// idx_bytes)` — the shape a [`crate::registry::PackRegistry`] record
/// needs. Reuses gitoxide's own indexer
/// (`gix_pack::Bundle::write_to_directory`, the same call
/// [`crate::OdbTigris::stage_pack`] makes) rather than hand-rolling a
/// second `.idx` writer; `crate::NoThinBaseLookup` is safe to reuse here
/// for the same reason it is safe in `stage_pack`: every pack this module
/// writes is self-contained (whole objects only, no thin-pack bases).
///
/// # Errors
///
/// Returns an error if indexing fails or the resulting files cannot be
/// read back.
pub fn index_pack(pack_bytes: Vec<u8>) -> Result<(Vec<u8>, Vec<u8>)> {
    let scratch = tempfile::tempdir()?;
    let mut reader = std::io::BufReader::new(std::io::Cursor::new(pack_bytes));
    let outcome = gix_pack::Bundle::write_to_directory(
        &mut reader,
        Some(scratch.path()),
        &mut gix_features::progress::Discard,
        &std::sync::atomic::AtomicBool::new(false),
        None::<crate::NoThinBaseLookup>,
        gix_pack::bundle::write::Options {
            object_hash: gix_hash::Kind::Sha1,
            ..Default::default()
        },
    )
    .map_err(|error| crate::Error::ObjectStore(error.to_string()))?;
    let data_path = outcome
        .data_path
        .ok_or_else(|| crate::Error::ObjectStore("pack write produced no data file".to_owned()))?;
    let index_path = outcome
        .index_path
        .ok_or_else(|| crate::Error::ObjectStore("pack write produced no index file".to_owned()))?;
    Ok((std::fs::read(data_path)?, std::fs::read(index_path)?))
}

/// Encode `objects` as a version-2 pack, every entry a full base object
/// (mirrors `git-protocol::pack::build_pack`, this crate's precedent for
/// "gix-pack's writer only does whole objects" — see this module's doc
/// comment for why). Returns `None` for an empty slice rather than an
/// empty-but-valid pack: callers only stage a pack that has at least one
/// object in it.
fn pack_whole_objects(objects: &[ClassifiedObject]) -> Result<Option<Vec<u8>>> {
    if objects.is_empty() {
        return Ok(None);
    }
    let entries: Vec<Entry> = objects
        .iter()
        .map(|object| {
            let count = Count::from_data(object.id, None);
            let data = gix_object::Data::new(&object.data, object.kind, gix_hash::Kind::Sha1);
            Entry::from_data(&count, &data)
                .map_err(|error| crate::Error::ObjectStore(error.to_string()))
        })
        .collect::<Result<_>>()?;
    let num_entries = u32::try_from(entries.len()).map_err(|_too_many| {
        crate::Error::ObjectStore("too many objects for one pack".to_owned())
    })?;
    let input = std::iter::once(Ok::<_, std::convert::Infallible>(entries));
    let mut writer = FromEntriesIter::new(
        input,
        Vec::new(),
        num_entries,
        Version::V2,
        gix_hash::Kind::Sha1,
    );
    for step in &mut writer {
        step.map_err(|error| crate::Error::ObjectStore(error.to_string()))?;
    }
    Ok(Some(writer.into_write()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "unit test")]

    use gix_hash::ObjectId;

    use super::*;

    fn blob(id_seed: u8, data: Vec<u8>, lifetime: LifetimeClass) -> ClassifiedObject {
        let mut bytes = [0u8; 20];
        bytes[0] = id_seed;
        ClassifiedObject {
            id: ObjectId::from(bytes),
            kind: Kind::Blob,
            data,
            lifetime,
        }
    }

    #[test]
    fn empty_partitions_produce_no_pack() {
        let result = partition_and_pack(Vec::new()).expect("partition");
        assert_eq!(result, PartitionedPacks::default());
    }

    #[test]
    fn durable_and_cache_objects_land_in_separate_packs() {
        let objects = vec![
            blob(1, b"durable content".to_vec(), LifetimeClass::Durable),
            blob(2, b"cache content".to_vec(), LifetimeClass::Cache),
        ];
        let result = partition_and_pack(objects).expect("partition");
        assert!(result.durable.is_some());
        assert!(result.cache.is_some());
        assert_ne!(result.durable, result.cache);
    }

    #[test]
    fn content_class_splits_blobs_by_size_threshold() {
        let small = blob(1, vec![0u8; 4], LifetimeClass::Durable);
        let large = blob(
            2,
            vec![0u8; BINARY_THRESHOLD_BYTES + 1],
            LifetimeClass::Durable,
        );
        assert_eq!(small.content_class(), ContentClass::Structural);
        assert_eq!(large.content_class(), ContentClass::Binary);
    }
}
