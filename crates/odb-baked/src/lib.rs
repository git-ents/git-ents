//! A read-only [`ObjectStore`] tier over a directory baked into a machine
//! image, keyed by manifest hash (`docs/scale-out.adoc`, "WS8 — Hydration
//! and toolchains", correctness rules 4 and 6).
//!
//! [`BakedTier`] composes in front of any other `ObjectStore` (a warm cache,
//! a cold-fetching remote tier) the same way [`odb_tiered::OdbTiered`]
//! composes its small-object tier: `read`/`contains` try the baked
//! directory first and fall through on a miss. The directory is written
//! once, by [`write`] (a bake effect, out of band — the baked tier itself
//! never accepts writes through the trait), and holds every object one
//! toolchain manifest's document tree reaches, named by object id so a
//! lookup is a filesystem stat plus a read, never a hash recomputation —
//! "verification on match is an ID comparison" per the design doc.
//!
//! Nothing here branches on *why* an object was found where it was found:
//! [`ObjectStore::read`]/[`ObjectStore::contains`] behave identically
//! whether the baked directory is present, stale, or absent, so
//! materialization stays the one code path rule 6 requires. Staleness
//! detection ([`BakedTier::verify_manifest`]) is a separate, explicit call
//! a caller makes purely for instrumentation (the hit/miss/stale counters
//! this type also exposes) — it never gates `read`/`contains`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use git_backend::{Error, Object, ObjectStore, PackStream, QuarantineId, Result};
use gix_hash::ObjectId;
use gix_object::Kind;

/// The file under a baked directory recording the manifest hash (the
/// toolchain document's root tree object id, hex-encoded) the directory was
/// baked for.
pub const MANIFEST_FILE: &str = "MANIFEST";

/// The subdirectory holding one file per baked object, sharded by the first
/// two hex characters of its id (mirroring loose-object fanout, though the
/// on-disk format itself is this crate's own, not git's).
pub const OBJECTS_DIR: &str = "objects";

/// Point-in-time hit/miss/stale counts off a [`BakedTier`], per
/// `docs/scale-out.adoc`'s WS8 instruction to instrument the miss rate — "a
/// stale image must not degrade silently."
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    /// Reads (`read` or `contains`) the baked directory answered directly.
    pub hits: u64,
    /// Reads the baked directory was consulted for but did not hold,
    /// falling through to the underlying tier.
    pub misses: u64,
    /// [`BakedTier::verify_manifest`] calls that found the baked directory
    /// present but baked for a different manifest than requested.
    pub stale: u64,
}

/// The outcome of comparing a requested manifest hash against what a
/// [`BakedTier`] was actually baked for, from [`BakedTier::verify_manifest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// The baked directory's manifest matches what was requested: the
    /// "verification is a single ID comparison" case.
    Fresh,
    /// The baked directory holds a different manifest than requested — the
    /// image is stale. `baked` is what it actually holds.
    Stale {
        /// The manifest hash the baked directory actually holds.
        baked: ObjectId,
    },
    /// No baked directory (or no [`MANIFEST_FILE`] in it) is present at
    /// all — a base image with nothing baked in, not staleness.
    Unbaked,
}

/// A read-only [`ObjectStore`] tier over a directory baked into a machine
/// image, composed in front of `underlying`. See the module doc.
pub struct BakedTier<S> {
    dir: PathBuf,
    manifest: Option<ObjectId>,
    underlying: S,
    hits: AtomicU64,
    misses: AtomicU64,
    stale: AtomicU64,
}

impl<S: ObjectStore> BakedTier<S> {
    /// Open the baked tier at `dir`, composed in front of `underlying`.
    /// `dir` need not exist, and need not hold [`MANIFEST_FILE`] — either
    /// case is treated as "nothing baked here" (a plain passthrough to
    /// `underlying`, never an error): a base image with no bake is an
    /// ordinary, expected deployment shape, not a fault.
    pub fn open(dir: &Path, underlying: S) -> Result<Self> {
        let manifest = read_manifest(dir)?;
        Ok(Self {
            dir: dir.to_owned(),
            manifest,
            underlying,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            stale: AtomicU64::new(0),
        })
    }

    /// This tier's current hit/miss/stale counts.
    #[must_use]
    pub fn counters(&self) -> Counters {
        Counters {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            stale: self.stale.load(Ordering::Relaxed),
        }
    }

    /// Compare `requested` (a materialization's "OID lookup" step result —
    /// the toolchain document's resolved root tree id) against the
    /// manifest hash this directory was actually baked for, bumping the
    /// [`Counters::stale`] counter and logging a warning on a mismatch, per
    /// the design doc: "a stale image must not degrade silently."
    ///
    /// Purely instrumentation: the result does not gate
    /// [`ObjectStore::read`]/[`ObjectStore::contains`], which fall through
    /// on a miss regardless of whether this was ever called — that keeps
    /// materialization one code path (rule 6): nothing above this tier
    /// branches on the answer here, only logs it.
    pub fn verify_manifest(&self, requested: ObjectId) -> Freshness {
        match self.manifest {
            None => Freshness::Unbaked,
            Some(baked) if baked == requested => Freshness::Fresh,
            Some(baked) => {
                self.stale.fetch_add(1, Ordering::Relaxed);
                eprintln!(
                    "odb-baked: baked tier at {} was baked for manifest {baked} but {requested} was requested; falling through",
                    self.dir.display()
                );
                Freshness::Stale { baked }
            }
        }
    }

    /// The object at `id` in the baked directory, or `None` if this tier
    /// has nothing baked, or nothing baked under `id` specifically.
    fn baked_object(&self, id: ObjectId) -> Option<Object> {
        self.manifest?;
        let bytes = fs::read(object_path(&self.dir, id)).ok()?;
        let (&kind_byte, data) = bytes.split_first()?;
        let kind = kind_from_byte(kind_byte)?;
        Some(Object {
            kind,
            data: data.to_vec(),
        })
    }
}

impl<S: ObjectStore> ObjectStore for BakedTier<S> {
    fn read(&self, id: ObjectId) -> Result<Object> {
        if self.manifest.is_some() {
            if let Some(object) = self.baked_object(id) {
                self.hits.fetch_add(1, Ordering::Relaxed);
                return Ok(object);
            }
            self.misses.fetch_add(1, Ordering::Relaxed);
        }
        self.underlying.read(id)
    }

    fn contains(&self, id: ObjectId) -> Result<bool> {
        if self.manifest.is_some() {
            if self.baked_object(id).is_some() {
                self.hits.fetch_add(1, Ordering::Relaxed);
                return Ok(true);
            }
            self.misses.fetch_add(1, Ordering::Relaxed);
        }
        self.underlying.contains(id)
    }

    // The baked tier is read-only: it is populated out of band by a bake
    // effect (see [`write`]), never through this trait. Writes pass
    // straight through to `underlying`, which is where every other tier
    // (warm cache, cold fetch) already commits staged objects — a baked
    // image is immutable for its lifetime.
    fn stage_pack(&self, pack: PackStream) -> Result<QuarantineId> {
        self.underlying.stage_pack(pack)
    }

    fn promote(&self, q: QuarantineId) -> Result<()> {
        self.underlying.promote(q)
    }
}

/// Write `objects` (and `manifest`, the toolchain document's root tree id
/// they were reached from) into the baked-tier directory layout at `dest`,
/// ready for [`BakedTier::open`] to serve — the bake effect's local half
/// (`crates/git-toolchain`'s `bake` module drives the toolchain-specific
/// tree walk; this function only knows about raw objects).
///
/// Idempotent and content-addressed: an object already on disk under its
/// own id is never rewritten, so baking the same manifest twice (or baking
/// two manifests that happen to share objects) does no redundant I/O.
pub fn write(
    dest: &Path,
    manifest: ObjectId,
    objects: impl IntoIterator<Item = (ObjectId, Kind, Vec<u8>)>,
) -> Result<()> {
    fs::create_dir_all(dest.join(OBJECTS_DIR))?;
    for (id, kind, data) in objects {
        let path = object_path(dest, id);
        if path.exists() {
            continue;
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut bytes = Vec::with_capacity(data.len().saturating_add(1));
        bytes.push(kind_to_byte(kind));
        bytes.extend_from_slice(&data);
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, &bytes)?;
        fs::rename(&tmp, &path)?;
    }
    let tmp_manifest = dest.join(MANIFEST_FILE).with_extension("tmp");
    fs::write(&tmp_manifest, manifest.to_string())?;
    fs::rename(&tmp_manifest, dest.join(MANIFEST_FILE))?;
    Ok(())
}

/// Read `dir`'s baked manifest hash, or `None` if `dir` (or its
/// [`MANIFEST_FILE`]) does not exist.
fn read_manifest(dir: &Path) -> Result<Option<ObjectId>> {
    let path = dir.join(MANIFEST_FILE);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    ObjectId::from_hex(text.trim().as_bytes())
        .map(Some)
        .map_err(|error| Error::ObjectStore(format!("{}: {error}", path.display())))
}

/// The on-disk path for object `id` under baked directory `dir`, sharded by
/// the id's first two hex characters.
fn object_path(dir: &Path, id: ObjectId) -> PathBuf {
    let hex = id.to_string();
    let (shard, rest) = hex.split_at(2.min(hex.len()));
    dir.join(OBJECTS_DIR).join(shard).join(rest)
}

/// Encode `kind` as this crate's one-byte object-kind tag.
fn kind_to_byte(kind: Kind) -> u8 {
    match kind {
        Kind::Blob => 0,
        Kind::Tree => 1,
        Kind::Commit => 2,
        Kind::Tag => 3,
    }
}

/// Decode this crate's one-byte object-kind tag, or `None` for an
/// unrecognized byte (a corrupt or foreign file under [`OBJECTS_DIR`]).
fn kind_from_byte(byte: u8) -> Option<Kind> {
    match byte {
        0 => Some(Kind::Blob),
        1 => Some(Kind::Tree),
        2 => Some(Kind::Commit),
        3 => Some(Kind::Tag),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::unwrap_in_result,
        clippy::let_underscore_must_use,
        reason = "unit test"
    )]

    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;

    /// A trivial in-memory [`ObjectStore`] test double, so these tests
    /// don't need a real repository — just enough to prove `BakedTier`
    /// falls through (and delegates writes) correctly.
    #[derive(Default)]
    struct MemStore {
        objects: Mutex<HashMap<ObjectId, Object>>,
        reads: AtomicU64,
    }

    impl MemStore {
        fn with(objects: impl IntoIterator<Item = (ObjectId, Object)>) -> Self {
            Self {
                objects: Mutex::new(objects.into_iter().collect()),
                reads: AtomicU64::new(0),
            }
        }
    }

    impl ObjectStore for MemStore {
        fn read(&self, id: ObjectId) -> Result<Object> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            self.objects
                .lock()
                .expect("lock")
                .get(&id)
                .cloned()
                .ok_or_else(|| Error::ObjectStore(format!("{id} not found")))
        }

        fn contains(&self, id: ObjectId) -> Result<bool> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            Ok(self.objects.lock().expect("lock").contains_key(&id))
        }

        fn stage_pack(&self, _pack: PackStream) -> Result<QuarantineId> {
            Ok(QuarantineId::new("mem"))
        }

        fn promote(&self, _q: QuarantineId) -> Result<()> {
            Ok(())
        }
    }

    fn oid(byte: u8) -> ObjectId {
        ObjectId::from_bytes_or_panic(&[byte; 20])
    }

    #[test]
    fn hits_the_baked_object_without_touching_the_underlying_store() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = oid(1);
        let blob = oid(2);
        write(
            dir.path(),
            manifest,
            [(blob, Kind::Blob, b"hello".to_vec())],
        )
        .expect("write");

        let underlying = MemStore::default();
        let tier = BakedTier::open(dir.path(), underlying).expect("open");
        let object = tier.read(blob).expect("read");
        assert_eq!(object.data, b"hello");
        assert!(tier.contains(blob).expect("contains"));

        let counters = tier.counters();
        assert_eq!(counters.hits, 2, "one hit from read, one from contains");
        assert_eq!(counters.misses, 0);
        // The underlying store was never consulted for the baked object.
        assert_eq!(tier.underlying.reads.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn falls_through_to_the_underlying_store_on_a_miss() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = oid(1);
        let baked_blob = oid(2);
        write(
            dir.path(),
            manifest,
            [(baked_blob, Kind::Blob, b"baked".to_vec())],
        )
        .expect("write");

        let elsewhere = oid(3);
        let underlying = MemStore::with([(
            elsewhere,
            Object {
                kind: Kind::Blob,
                data: b"cold-fetched".to_vec(),
            },
        )]);
        let tier = BakedTier::open(dir.path(), underlying).expect("open");

        let object = tier.read(elsewhere).expect("read falls through");
        assert_eq!(object.data, b"cold-fetched");

        let counters = tier.counters();
        assert_eq!(counters.hits, 0);
        assert_eq!(counters.misses, 1);
        assert_eq!(tier.underlying.reads.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn passthrough_when_nothing_is_baked() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = oid(4);
        let underlying = MemStore::with([(
            target,
            Object {
                kind: Kind::Blob,
                data: b"only in underlying".to_vec(),
            },
        )]);
        // `dir` exists but was never baked (no MANIFEST) — must behave
        // identically to a base image with no baked tier at all.
        let tier = BakedTier::open(dir.path(), underlying).expect("open");
        let object = tier.read(target).expect("read");
        assert_eq!(object.data, b"only in underlying");
        // A passthrough tier's absence of a bake must not be counted as a
        // miss — there was nothing to miss.
        assert_eq!(tier.counters(), Counters::default());
    }

    #[test]
    fn verify_manifest_reports_fresh_stale_and_unbaked() {
        let dir = tempfile::tempdir().expect("tempdir");
        let baked_for = oid(1);
        write(dir.path(), baked_for, []).expect("write");
        let tier = BakedTier::open(dir.path(), MemStore::default()).expect("open");

        assert_eq!(tier.verify_manifest(baked_for), Freshness::Fresh);
        assert_eq!(tier.counters().stale, 0);

        let other = oid(9);
        assert_eq!(
            tier.verify_manifest(other),
            Freshness::Stale { baked: baked_for }
        );
        assert_eq!(tier.counters().stale, 1);

        let unbaked_dir = tempfile::tempdir().expect("tempdir");
        let unbaked = BakedTier::open(unbaked_dir.path(), MemStore::default()).expect("open");
        assert_eq!(unbaked.verify_manifest(baked_for), Freshness::Unbaked);
    }

    #[test]
    fn writes_and_promotes_delegate_straight_to_the_underlying_store() {
        let dir = tempfile::tempdir().expect("tempdir");
        let underlying = MemStore::default();
        let tier = BakedTier::open(dir.path(), underlying).expect("open");

        let quarantine = tier
            .stage_pack(PackStream::new(std::io::Cursor::new(Vec::new())))
            .expect("stage_pack");
        tier.promote(quarantine).expect("promote");
    }

    #[test]
    fn one_code_path_same_materialization_result_with_and_without_the_baked_tier() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = oid(1);
        let blob = oid(2);
        let content = b"identical either way".to_vec();
        write(dir.path(), manifest, [(blob, Kind::Blob, content.clone())]).expect("write");

        let with_baked = BakedTier::open(
            dir.path(),
            MemStore::with([(
                blob,
                Object {
                    kind: Kind::Blob,
                    data: content.clone(),
                },
            )]),
        )
        .expect("open");
        let without_baked = MemStore::with([(
            blob,
            Object {
                kind: Kind::Blob,
                data: content.clone(),
            },
        )]);

        let via_baked = with_baked.read(blob).expect("read via baked tier");
        let via_underlying = without_baked.read(blob).expect("read via underlying alone");
        assert_eq!(via_baked, via_underlying);
        assert_eq!(via_baked.data, content);
        // No effect can observe which tier answered: the baked tier hit,
        // the plain store didn't, and both produced the same `Object`.
        assert_eq!(with_baked.counters().hits, 1);
    }
}
