//! Mark-and-sweep GC (`docs/scale-out.adoc`, WS9: "Mark from RefStore via
//! reachability artifacts; sweep via pack registry").
//!
//! # Mark
//!
//! [`collect`] marks with [`gix_reachability::gc_mark`] — every object
//! reachable from every current ref tip, accelerated by whatever
//! reachability artifacts the repo has (absence degrades speed, never
//! answers). A second, durable-tips-only walk splits the marked set into
//! lifetime classes ([`odb_tigris::pack_writer::LifetimeClass`]) so the
//! sweep's repack path can honor the pack-lifetime rule (rule 5).
//!
//! # Sweep — and why quarantine is structurally safe
//!
//! The sweep enumerates [`odb_tigris::registry::PackRegistry::list`] and
//! nothing else. Quarantined (staged) packs are *not in the registry* —
//! [`odb_tigris::OdbTigris::promote`] is what records a pack, and it is
//! only called after the ref transaction the pack was staged for commits —
//! and [`odb_tigris::transport::BlobTransport`] exposes no listing call at
//! all, so there is no API through which this module *could* scan
//! quarantine (correctness rules 1 and 2: "GC never scans quarantine").
//! That safety is structural, not a filter this code must remember to
//! apply.
//!
//! Grace-based staging (rule 1's time-bounded arm) lives on the store
//! itself: [`odb_tigris::OdbTigris::with_staging_grace`] bounds staging
//! sessions (a session past its window aborts at `promote` rather than
//! becoming collectible mid-flight), and
//! [`odb_tigris::OdbTigris::expire_stale_quarantines`] is the cruft pass a
//! grace-based collector runs — see [`crate::collector::TigrisCollector`].
//!
//! # Sweep outcomes per pack
//!
//! - every object unreachable → **delete**: registry delete (the commit
//!   point), then best-effort blob deletes.
//! - every object reachable → keep.
//! - mixed, with at least one durable reachable object → **rewrite**:
//!   reachable objects are repacked through the WS5 pack writer
//!   ([`odb_tigris::pack_writer::partition_and_pack`], which partitions by
//!   lifetime class so cache and durable objects never share the new
//!   pack), the new pack(s) are recorded, and only then is the old pack
//!   deleted — no window where a live object is unregistered.
//! - mixed, all reachable objects cache-lifetime → left whole: cache packs
//!   die by registry delete when their refs are evicted, never repack
//!   surgery (rule 5).

use std::collections::BTreeSet;

use git_backend::cache_ns;
use git_backend::{ObjectStore, RefName, RefStore};
use gix_hash::ObjectId;
use gix_reachability::walk::StoreSource;
use odb_tigris::pack_writer::{ClassifiedObject, LifetimeClass, index_pack, partition_and_pack};
use odb_tigris::registry::{PackId, PackRecord, PackRegistry};
use odb_tigris::transport::BlobTransport;

use crate::{Error, Result};

/// What one [`collect`] pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GcOutcome {
    /// Packs whose objects were all unreachable, deleted whole.
    pub deleted_packs: usize,
    /// Mixed packs rewritten to contain only their reachable objects.
    pub rewritten_packs: usize,
    /// The size of the marked (reachable) set.
    pub live_objects: usize,
}

/// One full mark-and-sweep pass for `repo_id` (see the module docs for the
/// mark/sweep design and the structural quarantine-safety argument).
///
/// # Errors
///
/// Returns an error if the mark walk fails (a ref tip whose history is
/// incomplete is corruption, never grounds to collect), or if a registry
/// or transport operation the sweep depends on fails.
pub fn collect(
    repo_id: &str,
    refs: &dyn RefStore,
    objects: &dyn ObjectStore,
    transport: &dyn BlobTransport,
    registry: &dyn PackRegistry,
) -> Result<GcOutcome> {
    let artifacts = gix_reachability::store::load_bundle(transport, registry, repo_id)
        .map_err(|error| Error::ObjectStore(error.to_string()))?;
    let marked = gix_reachability::gc_mark(refs, objects, &artifacts)
        .map_err(|error| Error::ObjectStore(error.to_string()))?;
    let durable = durable_reachable(refs, objects, &artifacts)?;

    let mut outcome = GcOutcome {
        live_objects: marked.len(),
        ..GcOutcome::default()
    };

    for record in registry.list(repo_id)? {
        let ids = pack_object_ids(transport, &record)?;
        let live: Vec<ObjectId> = ids
            .iter()
            .filter(|id| marked.contains(*id))
            .copied()
            .collect();

        if live.is_empty() {
            // Registry delete first — it is the commit point; blob deletes
            // after it are best-effort cleanup (a leaked key wastes space,
            // never correctness), mirroring `OdbTigris::promote`.
            registry.delete(repo_id, &record.id)?;
            let _ignored = transport.delete(&record.pack_key);
            let _ignored = transport.delete(&record.idx_key);
            outcome.deleted_packs = outcome.deleted_packs.saturating_add(1);
            continue;
        }
        if live.len() == ids.len() {
            continue;
        }

        // Mixed pack. A pack whose reachable objects are all
        // cache-lifetime is a cache pack: never repack surgery (rule 5) —
        // it dies whole once its cache refs are evicted.
        if live.iter().all(|id| !durable.contains(id)) {
            continue;
        }

        rewrite_pack(
            repo_id, objects, transport, registry, &record, &live, &durable,
        )?;
        outcome.rewritten_packs = outcome.rewritten_packs.saturating_add(1);
    }
    Ok(outcome)
}

/// The objects reachable from durable (non-cache) ref tips alone — the
/// lifetime-class oracle for repack partitioning: marked objects in this
/// set are [`LifetimeClass::Durable`], marked objects outside it are
/// reachable only through cache refs and so [`LifetimeClass::Cache`].
fn durable_reachable(
    refs: &dyn RefStore,
    objects: &dyn ObjectStore,
    artifacts: &gix_reachability::ArtifactBundle,
) -> Result<BTreeSet<ObjectId>> {
    let tips = refs
        .iter_prefix(&RefName::new("refs/"))?
        .filter(|entry| match entry {
            Ok((name, _oid)) => !cache_ns::is_cache_ref(name),
            Err(_error) => true,
        })
        .map(|entry| entry.map(|(_name, oid)| oid))
        .collect::<git_backend::Result<Vec<ObjectId>>>()?;
    let source = StoreSource::new(objects);
    gix_reachability::accelerated_reachable(tips, &source, |_id| false, false, artifacts)
        .map_err(|error| Error::ObjectStore(error.to_string()))
}

/// Every object id in `record`'s pack, read from its `.idx` — never from a
/// bucket listing (the transport has none to offer).
fn pack_object_ids(transport: &dyn BlobTransport, record: &PackRecord) -> Result<Vec<ObjectId>> {
    let bytes = transport.get(&record.idx_key)?;
    let idx = gix_pack::index::File::from_data(
        bytes,
        std::path::PathBuf::from(&record.idx_key),
        gix_hash::Kind::Sha1,
    )
    .map_err(|error| Error::ObjectStore(format!("parsing index {}: {error}", record.idx_key)))?;
    Ok(idx.iter().map(|entry| entry.oid).collect())
}

/// Repack `live` (the reachable objects of a mixed pack) into fresh
/// pack(s) through the WS5 pack writer — partitioned by lifetime class, so
/// the pack-lifetime rule survives the rewrite — record them, and only
/// then delete the old pack.
fn rewrite_pack(
    repo_id: &str,
    objects: &dyn ObjectStore,
    transport: &dyn BlobTransport,
    registry: &dyn PackRegistry,
    record: &PackRecord,
    live: &[ObjectId],
    durable: &BTreeSet<ObjectId>,
) -> Result<()> {
    let classified: Vec<ClassifiedObject> = live
        .iter()
        .map(|id| {
            let object = objects.read(*id)?;
            Ok(ClassifiedObject {
                id: *id,
                kind: object.kind,
                data: object.data,
                lifetime: if durable.contains(id) {
                    LifetimeClass::Durable
                } else {
                    LifetimeClass::Cache
                },
            })
        })
        .collect::<Result<_>>()?;

    let packs = partition_and_pack(classified)?;
    for pack_bytes in [packs.durable, packs.cache].into_iter().flatten() {
        record_new_pack(repo_id, transport, registry, pack_bytes)?;
    }

    registry.delete(repo_id, &record.id)?;
    let _ignored = transport.delete(&record.pack_key);
    let _ignored = transport.delete(&record.idx_key);
    Ok(())
}

/// Index freshly written pack bytes, upload them at new live keys, and
/// record them — the same key layout `OdbTigris` promotes into.
fn record_new_pack(
    repo_id: &str,
    transport: &dyn BlobTransport,
    registry: &dyn PackRegistry,
    pack_bytes: Vec<u8>,
) -> Result<()> {
    let (pack, idx) = index_pack(pack_bytes)?;
    let object_count = count_pack_objects(&idx);
    let id = uuid::Uuid::new_v4().to_string();
    let pack_key = format!("{repo_id}/live/{id}.pack");
    let idx_key = format!("{repo_id}/live/{id}.idx");
    transport.put(&pack_key, pack)?;
    transport.put(&idx_key, idx)?;
    registry.record(PackRecord {
        id: PackId::new(id),
        repo_id: repo_id.to_owned(),
        pack_key,
        idx_key,
        object_count,
    })
}

/// The object count out of freshly written `.idx` bytes, or `None` if they
/// fail to parse — the count is informational only ([`PackRecord`]'s field
/// docs), so an unparsable count is not worth failing a rewrite over.
fn count_pack_objects(idx_bytes: &[u8]) -> Option<u64> {
    gix_pack::index::File::from_data(
        idx_bytes.to_vec(),
        std::path::PathBuf::from("rewrite.idx"),
        gix_hash::Kind::Sha1,
    )
    .ok()
    .map(|idx| u64::from(idx.num_objects()))
}

/// What one [`collect_files`] pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FilesGcOutcome {
    /// Packs under `objects/pack/` whose objects were all unreachable,
    /// deleted whole.
    pub deleted_packs: usize,
    /// The size of the marked (reachable) set.
    pub live_objects: usize,
}

/// Mark-and-sweep for the local files backend (`refstore-files` +
/// `odb-files`): mark from ref tips, then delete every pack under
/// `objects/pack/` whose objects are all unreachable. Whole-pack reaping
/// only — the local backend has no pack registry to rewrite through, and
/// mixed packs are simply kept (correct, just less compact; the
/// registry-backed sweep is where rewriting lives).
///
/// Structurally quarantine-safe for the same reason `odb-files` itself is:
/// this sweep scans `objects/pack/` and nothing else, and staged packs
/// live under `objects/quarantine/<id>/` until promoted.
///
/// # Errors
///
/// Returns an error if the repository cannot be opened, the mark walk
/// fails, or a doomed pack cannot be deleted.
pub fn collect_files(repo: &std::path::Path) -> Result<FilesGcOutcome> {
    let (marked, doomed) = {
        let refs = refstore_files::FilesRefStore::open(repo)?;
        let objects = odb_files::OdbFiles::open(repo)?;
        let marked =
            gix_reachability::gc_mark(&refs, &objects, &gix_reachability::ArtifactBundle::empty())
                .map_err(|error| Error::ObjectStore(error.to_string()))?;

        let pack_dir = repo.join("objects").join("pack");
        let mut doomed = Vec::new();
        if pack_dir.is_dir() {
            for entry in std::fs::read_dir(&pack_dir)? {
                let path = entry?.path();
                if path.extension().is_some_and(|ext| ext == "idx") {
                    let idx = gix_pack::index::File::at(&path, gix_hash::Kind::Sha1).map_err(
                        |error| {
                            Error::ObjectStore(format!("parsing index {}: {error}", path.display()))
                        },
                    )?;
                    if idx.iter().all(|entry| !marked.contains(&entry.oid)) {
                        doomed.push(path);
                    }
                }
            }
        }
        (marked, doomed)
        // `objects` (and its pack mmaps) drop here, before any deletion.
    };

    let mut outcome = FilesGcOutcome {
        live_objects: marked.len(),
        ..FilesGcOutcome::default()
    };
    for idx_path in doomed {
        std::fs::remove_file(&idx_path)?;
        let pack_path = idx_path.with_extension("pack");
        if pack_path.exists() {
            std::fs::remove_file(&pack_path)?;
        }
        outcome.deleted_packs = outcome.deleted_packs.saturating_add(1);
    }
    Ok(outcome)
}
