//! Baking a toolchain manifest into the WS8 baked-tier directory layout
//! (`docs/scale-out.adoc`, "WS8 — Hydration and toolchains"): the layout a
//! read-only [`odb_baked::BakedTier`] serves directly once it is baked into
//! a machine image.
//!
//! Image bake is itself meant to be an attested effect — the design doc is
//! explicit that the baked tier must not be a hole in the trust story.
//! [`bake`] is the local half this repository can honestly drive:
//! materialize the layout and compute the manifest hash it was baked for.
//! [`record`] lands that hash so a later reader can tell which manifest a
//! given bake actually covers, by writing a plain document through
//! [`git_store::Store`] — the same local write [`crate::results`]-style
//! bookkeeping elsewhere in this repository uses. The *attested* half of
//! "attested effect" is not reimplemented here: a bake runs as an ordinary
//! effect (see [`effect_def`]), which the WS7 dispatcher already spawns
//! with the worker's own member key wired onto `user.signingkey`, so
//! whatever pushes this record out to a server — the CLI's own
//! `push_signed`, an effect's own push step — crosses the identical
//! `pre-receive` gate any other write does. Assembling and publishing the
//! actual Fly machine image from a bake's output is deploy-time
//! infrastructure this crate cannot honestly claim to drive; nothing here
//! pretends otherwise.

use std::collections::HashSet;
use std::path::Path;

use facet::Facet;
use gix::ObjectId;
use gix::objs::FindExt as _;
use gix::objs::tree::EntryKind;

use crate::{Error, odb_at, toolchain_ref};

/// The ref namespace recording each toolchain's most recently baked
/// manifest hash: `refs/meta/toolchains/baked/<name>`. Distinct from
/// [`crate::TOOLCHAINS_NS`] itself (`import`'s own history) — a bake is a
/// *materialization* of an already-imported toolchain, recorded separately
/// so publishing a stale bake never rewrites the toolchain's own
/// provenance.
pub const BAKED_NS: &str = "refs/meta/toolchains/baked";

/// The ref holding toolchain `name`'s baked-manifest record.
#[must_use]
pub fn baked_ref(name: &str) -> String {
    format!("{BAKED_NS}/{name}")
}

/// The record [`record`] lands: which manifest hash was baked, and when —
/// what a later reader (or `odb_baked::BakedTier::verify_manifest`'s
/// caller) checks a running image's bake against.
#[derive(Debug, Clone, PartialEq, Facet)]
pub struct BakedRecord {
    /// The toolchain document's root tree object id, hex-encoded — the
    /// same "manifest hash" [`odb_baked::BakedTier`] is keyed by.
    pub manifest: String,
    /// When the bake was recorded, seconds since the Unix epoch.
    pub baked_at: u64,
}

/// Materialize toolchain `name`'s full object closure — its document tree
/// (`refs/meta/toolchains/<name>`'s tip, recursively: `bin`, `src`, and
/// whatever scalar fields `facet_git_tree` laid out alongside them) — into
/// the baked-tier directory layout at `dest`, via [`odb_baked::write`].
///
/// Returns the manifest hash: the resolved document's root tree object id,
/// the same id a materialization's "OID lookup" step produces and
/// [`odb_baked::BakedTier::verify_manifest`] compares against. Keying the
/// bake on the *whole* document tree (not just `bin`) means a baked image
/// can serve every object [`crate::resolve`]/[`crate::export`] would
/// otherwise read from the repository's own object database, regardless of
/// whether the toolchain is [`crate::Bin::Embedded`] or
/// [`crate::Bin::Downloaded`].
pub fn bake(repo: &Path, name: &str, dest: &Path) -> Result<ObjectId, Error> {
    if !git_store::ref_segment_ok(name) {
        return Err(Error::InvalidName(name.to_owned()));
    }
    let store = git_store::Store::open(repo)?;
    let manifest = store.ref_tree(&toolchain_ref(name))?;
    let odb = odb_at(repo)?;

    let mut objects = Vec::new();
    let mut seen = HashSet::new();
    collect_closure(&odb, manifest, &mut objects, &mut seen)?;

    odb_baked::write(dest, manifest, objects)
        .map_err(|error| Error::Bake(dest.to_owned(), error.to_string()))?;
    Ok(manifest)
}

/// Recursively collect every tree and blob object `root` reaches (`root`
/// itself included) into `out`, deduplicated via `seen` — the object
/// closure [`bake`] hands to [`odb_baked::write`]. Mirrors
/// [`crate::export`]'s own tree walk ([`write_tree_to_disk`] in `lib.rs`),
/// but collects objects instead of writing them to disk, and skips
/// `EntryKind::Commit` (a submodule reference) the same way that walk does
/// — there is no object in this database to collect for it.
fn collect_closure(
    odb: &gix::odb::Handle,
    root: ObjectId,
    out: &mut Vec<(ObjectId, gix::objs::Kind, Vec<u8>)>,
    seen: &mut HashSet<ObjectId>,
) -> Result<(), Error> {
    if !seen.insert(root) {
        return Ok(());
    }
    let mut buf = Vec::new();
    // Owned `(kind, id)` pairs, not borrowed `EntryRef`s: `tree` ties its
    // borrow to `buf` for its whole lifetime, and `buf` is reused (cleared
    // and rewritten) by every recursive call below, so nothing borrowing it
    // may survive past this block.
    let children: Vec<(EntryKind, ObjectId)> = {
        let tree = odb
            .find_tree(&root, &mut buf)
            .map_err(|error| git_store::Error::Object(error.to_string()))?;
        tree.entries
            .iter()
            .map(|entry| (entry.mode.kind(), entry.oid.to_owned()))
            .collect()
    };
    out.push((root, gix::objs::Kind::Tree, buf.clone()));

    for (kind, child) in children {
        match kind {
            EntryKind::Tree => {
                collect_closure(odb, child, out, seen)?;
            }
            EntryKind::Commit => {
                // A submodule commit reference; nothing this object
                // database holds to collect.
            }
            EntryKind::Link | EntryKind::Blob | EntryKind::BlobExecutable => {
                if seen.insert(child) {
                    let mut blob_buf = Vec::new();
                    let blob = odb
                        .find_blob(&child, &mut blob_buf)
                        .map_err(|error| git_store::Error::Object(error.to_string()))?;
                    out.push((child, gix::objs::Kind::Blob, blob.data.to_vec()));
                }
            }
        }
    }
    Ok(())
}

/// Land `manifest` as toolchain `name`'s newly baked record onto
/// [`baked_ref`], as a plain local write through [`git_store::Store`] — the
/// same shape `crate` uses for every other typed document. Pushing this
/// record out to wherever `name`'s canonical history lives (attested,
/// worker-member-key-signed, per the module doc) is the caller's job, the
/// same way every other local `git_toolchain` write is pushed out by its
/// caller rather than by this crate.
pub fn record(repo: &Path, name: &str, manifest: ObjectId) -> Result<(), Error> {
    let record = BakedRecord {
        manifest: manifest.to_string(),
        baked_at: unix_now(),
    };
    let store = git_store::Store::open(repo)?;
    store.store(&baked_ref(name), &record, "Record baked manifest")?;
    Ok(())
}

/// The [`git_backend::EffectDef`] for baking toolchain `name` into `dest`,
/// following the same shape effects/dispatcher already spawn (WS6/WS7): a
/// shell command, no sandbox image override — a bake effect prepares the
/// very image other effects later run in, so it runs on the dispatcher's
/// own worker rather than inside a Sprite that would need it already built.
#[must_use]
pub fn effect_def(name: &str, dest: &str) -> git_backend::EffectDef {
    git_backend::EffectDef {
        name: format!("bake-toolchain-{name}"),
        command: Some(format!("git ents toolchain bake {name} {dest}")),
        image: None,
    }
}

/// Seconds since the Unix epoch, clamped to `0` on a clock before it (never
/// expected in practice) rather than panicking.
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::let_underscore_must_use,
        reason = "unit test"
    )]

    use git_store::test_support::repo;
    use odb_baked::BakedTier;
    use odb_files::OdbFiles;

    use super::*;

    #[test]
    fn bake_produces_a_layout_the_baked_tier_can_serve() {
        let repo_dir = repo();
        let import_dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(import_dir.path().join("tool"), b"#!/bin/sh\necho hi\n").expect("write");
        crate::import(
            repo_dir.path(),
            "demo",
            import_dir.path(),
            None,
            "MIT",
            "1.0.0",
            "x86_64-unknown-linux-gnu",
            None,
        )
        .expect("import");

        let baked_dir = tempfile::tempdir().expect("tempdir");
        let manifest = bake(repo_dir.path(), "demo", baked_dir.path()).expect("bake");

        let odb = OdbFiles::open(repo_dir.path()).expect("open odb");
        let tier = BakedTier::open(baked_dir.path(), odb).expect("open baked tier");
        assert_eq!(tier.verify_manifest(manifest), odb_baked::Freshness::Fresh);

        // The document tree itself, and everything it reaches, must be
        // servable straight from the baked tier.
        use git_backend::ObjectStore as _;
        assert!(tier.contains(manifest).expect("contains manifest"));
        let object = tier.read(manifest).expect("read manifest");
        assert_eq!(object.kind, gix::objs::Kind::Tree);
        assert_eq!(tier.counters().hits, 2);
    }

    #[test]
    fn record_lands_the_manifest_hash() {
        let repo_dir = repo();
        let import_dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(import_dir.path().join("tool"), b"bin").expect("write");
        crate::import(
            repo_dir.path(),
            "demo",
            import_dir.path(),
            None,
            "MIT",
            "1.0.0",
            "x86_64-unknown-linux-gnu",
            None,
        )
        .expect("import");

        let baked_dir = tempfile::tempdir().expect("tempdir");
        let manifest = bake(repo_dir.path(), "demo", baked_dir.path()).expect("bake");
        record(repo_dir.path(), "demo", manifest).expect("record");

        let store = git_store::Store::open(repo_dir.path()).expect("open store");
        let loaded: BakedRecord = store
            .load(&baked_ref("demo"))
            .expect("load")
            .expect("record present");
        assert_eq!(loaded.manifest, manifest.to_string());
    }
}
