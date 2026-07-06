//! Toolchains stored as a typed document over two raw-passthrough git trees.
//!
//! A toolchain is a directory tree (a compiler, an SDK, any reproducible
//! build environment) captured as ordinary git trees rather than shipped in
//! a container image, plus a license: [`import`] walks a local `bin`
//! directory (and, optionally, a `src` directory) and writes them as the tip
//! of `refs/meta/toolchains/<name>`, [`resolve`] reads that tip back as a
//! [`Toolchain`], and [`export`] walks a resolved toolchain's trees back onto
//! disk. There is no hardlink manager or blob store here — a Sprite extracts
//! a resolved `bin` tree once into a hash-keyed directory, and its
//! persistent filesystem is the cache.
//!
//! `bin` and `src` are each captured whole as a [`facet_git_tree::RawTree`]:
//! their internal layout is arbitrary and untyped, so `Toolchain` only
//! records the two trees' object ids and the license, rather than modeling
//! directory contents as `Facet` fields.
//!
//! Permissions beyond the executable bit are dropped and empty directories
//! are skipped (a git tree cannot represent either), so importing the same
//! directory contents on any machine writes the same tree hash. Large loose
//! objects are fine functionally; repacking the object database is an
//! operational follow-up, not something this crate does.

use std::collections::HashMap;
use std::fs;
use std::io::{Seek as _, SeekFrom, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::str::FromStr as _;
use std::sync::atomic::AtomicBool;
use std::sync::{Mutex, PoisonError};

use facet::Facet;
use facet_git_tree::RawTree;
use git_store::Store;
use gix::ObjectId;
use gix::bstr::ByteSlice as _;
use gix::objs::tree::{Entry as TreeEntry, EntryKind, EntryMode};
use gix::objs::{FindExt as _, Tree, WriteTo as _};
use gix::prelude::HeaderExt as _;
use gix_pack::data::input::Entry as PackEntry;
use rayon::prelude::*;

/// The ref namespace holding toolchains, one ref per toolchain:
/// `refs/meta/toolchains/<name>`. A toolchain's identity is its tip commit's
/// tree hash, so importing identical contents twice is a no-op churn-wise.
pub const TOOLCHAINS_NS: &str = "refs/meta/toolchains";

/// A toolchain: an executable `bin` directory, an optional `src` directory,
/// its license, version, and target platform — the document stored at the
/// tip of `refs/meta/toolchains/<name>`.
///
/// `src` is [`RawTree`]: captured as a single opaque git tree by [`import`],
/// not walked field-by-field, since a toolchain's on-disk layout has no fixed
/// shape for `Facet` to model. `bin` is either the same ([`Bin::Embedded`])
/// or a set of externally-hosted archives fetched fresh at activation or
/// export time ([`Bin::Downloaded`]) — see [`Bin`].
///
/// `license`, `version`, and `platform` are stored as plain strings — like
/// `license` before them, `version` and `platform` are validated against a
/// real parser (`semver`, `target-lexicon`) at [`import`] time rather than
/// carried as a parsed type, since nothing downstream needs more than the
/// canonical string back.
#[derive(Debug, Clone, PartialEq, Facet)]
pub struct Toolchain {
    /// The toolchain's executables, activated on `PATH` when a check
    /// requests it.
    pub bin: Bin,
    /// The toolchain's source, if imported — not activated on `PATH`, kept
    /// only for provenance.
    pub src: Option<RawTree>,
    /// The license covering `bin` (and `src`, if present), an SPDX license
    /// expression (`MIT`, `Apache-2.0 WITH LLVM-exception`, ...).
    pub license: String,
    /// The toolchain's version, a semver string.
    pub version: String,
    /// The toolchain's target platform, an LLVM/autotools-style target
    /// triple (`x86_64-unknown-linux-gnu`, ...) — the closest thing to a
    /// standard platform identifier; there is no SPDX-equivalent registry
    /// for platforms.
    pub platform: String,
    /// The recipe (and its selector) this import was derived from, e.g.
    /// `"rustup stable"` — `None` when `bin`/`src`/metadata were supplied by
    /// hand instead. Recorded here so the toolchain's current state names its
    /// own origin; [`history`] additionally surfaces every past import's
    /// recipe from the ref's commit log, not just the tip's.
    pub recipe: Option<String>,
}

/// How a toolchain's `bin` is provisioned.
#[derive(Debug, Clone, PartialEq, Facet)]
#[repr(u8)]
pub enum Bin {
    /// `bin`'s directory tree, captured whole in the object database by
    /// [`import`] — the only representation for a toolchain with no stable,
    /// independently-hosted origin (an in-house build).
    Embedded(RawTree),
    /// A set of archives fetched, sha256-verified, and merged onto disk by
    /// [`export`] (or a Sprite, at check-activation time) instead of stored
    /// in the object database — spares the repository the toolchain's own
    /// bytes when a stable, content-hashed origin (a distributor's release
    /// archives) already exists. Each component records its own archive
    /// layout ([`Component::strip`], [`Component::dest`]), so a rust-lang
    /// dist archive and a flat single-binary release tarball can coexist in
    /// one toolchain.
    Downloaded(Vec<Component>),
}

/// One archive making up a [`Bin::Downloaded`] toolchain: fetched from `url`
/// and checked against `sha256` before being extracted per `strip`/`dest`.
#[derive(Debug, Clone, PartialEq, Facet)]
pub struct Component {
    /// Where to fetch the archive from.
    pub url: String,
    /// The archive's expected sha256, hex-encoded — checked before
    /// extraction; a mismatch is refused rather than extracted anyway.
    pub sha256: String,
    /// Leading path segments stripped at extraction (`tar
    /// --strip-components`): 2 for rust-lang dist archives
    /// (`<package>-<version>-<target>/<component>/…`), 1 for a flat release
    /// tarball (`<package>-<version>/<binary>`).
    pub strip: u8,
    /// Subdirectory under the toolchain's extraction root to extract into:
    /// empty for archives already carrying their own `bin/`/`lib/`/… top
    /// level (rustup components), `bin` for flat archives whose payload
    /// should itself land on `PATH` — keeping `<root>/bin` the one activation
    /// convention either way.
    pub dest: String,
}

/// A failure importing, resolving, listing, exporting, or removing a
/// toolchain.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A `git-store` ref or object operation failed — opening the
    /// repository, resolving or deleting a toolchain's ref, or a raw object
    /// read/write this crate performs directly against the same object
    /// database `git-store` uses.
    #[error(transparent)]
    Store(#[from] git_store::Error),
    /// A [`Toolchain`] could not be (de)serialized from its git tree.
    #[error(transparent)]
    Facet(#[from] facet_git_tree::Error),
    /// `name` failed [`git_store::ref_segment_ok`].
    #[error("{0:?} is not a valid toolchain name")]
    InvalidName(String),
    /// A path under the imported or exported directory could not be read or
    /// written.
    #[error("could not access {0}: {1}")]
    Io(PathBuf, std::io::Error),
    /// A file or symlink name, or a symlink target, was not valid UTF-8.
    #[error("{0} is not valid UTF-8")]
    NotUtf8(PathBuf),
    /// [`export`]'s destination directory already has contents; refuses to
    /// clobber them.
    #[error("{0} already exists and is not empty")]
    DestNotEmpty(PathBuf),
    /// [`import`]'s `bin` directory produced no entries. A toolchain that
    /// activates nothing on `PATH` is not a toolchain.
    #[error("{0} contains nothing importable; a toolchain's bin directory must not be empty")]
    EmptyBin(PathBuf),
    /// [`import_downloaded`]'s component list was empty. A toolchain that
    /// activates nothing on `PATH` is not a toolchain.
    #[error("a downloaded toolchain must list at least one component")]
    NoComponents,
    /// A [`Component`] carried a field unsafe to interpolate into the shell
    /// script that fetches and extracts it: a `dest` that is not empty or a
    /// single safe path segment, or a `url`/`sha256` containing a quote.
    #[error("invalid component: {0}")]
    InvalidComponent(String),
    /// A [`Bin::Downloaded`] component could not be fetched or extracted.
    #[error("could not fetch {0}: {1}")]
    Fetch(String, String),
    /// A [`Bin::Downloaded`] component's fetched content did not match its
    /// recorded sha256.
    #[error("{0}: expected sha256 {1}, got {2}")]
    HashMismatch(String, String, String),
    /// [`import`]'s `license` argument was not a valid SPDX license
    /// expression.
    #[error("{0:?} is not a valid SPDX license expression: {1}")]
    InvalidLicense(String, spdx::ParseError),
    /// [`import`]'s `version` argument was not a valid semver version.
    #[error("{0:?} is not a valid semver version: {1}")]
    InvalidVersion(String, semver::Error),
    /// [`import`]'s `platform` argument was not a valid target triple.
    #[error("{0:?} is not a valid target triple")]
    InvalidPlatform(String),
}

/// Import `bin_dir` (and, optionally, `src_dir`) into `repo` as the
/// toolchain `name`: write each directory tree bottom-up into the object
/// database, assemble a [`Toolchain`] document over them, `license`,
/// `version`, and `platform`, and fast-forward `refs/meta/toolchains/<name>`
/// to a commit over it. Returns the document's root tree object id.
///
/// `license` MUST be a valid SPDX license expression, `version` a valid
/// semver version, and `platform` a valid target triple. `recipe`, if given,
/// is recorded on the [`Toolchain`] document and in the import's commit
/// message as this import's provenance (see [`Toolchain::recipe`]).
#[expect(clippy::too_many_arguments, reason = "one flag per import field")]
pub fn import(
    repo: &Path,
    name: &str,
    bin_dir: &Path,
    src_dir: Option<&Path>,
    license: &str,
    version: &str,
    platform: &str,
    recipe: Option<&str>,
) -> Result<ObjectId, Error> {
    if !git_store::ref_segment_ok(name) {
        return Err(Error::InvalidName(name.to_owned()));
    }
    validate_metadata(license, version, platform)?;
    let odb = odb_at(repo)?;
    let collector = PackCollector::default();

    let bin_tree = build_tree(&collector, bin_dir)?;
    if bin_tree.entries.is_empty() {
        return Err(Error::EmptyBin(bin_dir.to_owned()));
    }
    let bin = Bin::Embedded(RawTree::new(write_tree(&collector, &bin_tree)?));
    let src = import_src(&collector, src_dir)?;
    collector.flush(repo)?;

    let toolchain = Toolchain {
        bin,
        src,
        license: license.to_owned(),
        version: version.to_owned(),
        platform: platform.to_owned(),
        recipe: recipe.map(str::to_owned),
    };
    store_toolchain(repo, name, toolchain, &odb)
}

/// Import a toolchain whose `bin` is a set of externally-hosted archives
/// (see [`Bin::Downloaded`]) instead of a local directory: no tree is walked
/// or written for `bin` itself, only the component list and the rest of the
/// document. `src_dir`, if given, is still captured as a `RawTree` the usual
/// way — provenance-only content with no natural external origin to point at
/// instead.
#[expect(clippy::too_many_arguments, reason = "one flag per import field")]
pub fn import_downloaded(
    repo: &Path,
    name: &str,
    components: Vec<Component>,
    src_dir: Option<&Path>,
    license: &str,
    version: &str,
    platform: &str,
    recipe: Option<&str>,
) -> Result<ObjectId, Error> {
    if !git_store::ref_segment_ok(name) {
        return Err(Error::InvalidName(name.to_owned()));
    }
    if components.is_empty() {
        return Err(Error::NoComponents);
    }
    for component in &components {
        validate_component(component)?;
    }
    validate_metadata(license, version, platform)?;
    let odb = odb_at(repo)?;
    let collector = PackCollector::default();
    let src = import_src(&collector, src_dir)?;
    collector.flush(repo)?;

    let toolchain = Toolchain {
        bin: Bin::Downloaded(components),
        src,
        license: license.to_owned(),
        version: version.to_owned(),
        platform: platform.to_owned(),
        recipe: recipe.map(str::to_owned),
    };
    store_toolchain(repo, name, toolchain, &odb)
}

/// A [`Component`]'s fields end up interpolated into a shell script (the
/// Sprite-side fetch-and-extract in `git-effect`), so refuse anything that
/// could escape it: `dest` must be empty or one safe path segment, and no
/// field may contain a single quote.
fn validate_component(component: &Component) -> Result<(), Error> {
    if !component.dest.is_empty() && !git_store::ref_segment_ok(&component.dest) {
        return Err(Error::InvalidComponent(format!(
            "dest {:?} is not empty or a single safe path segment",
            component.dest
        )));
    }
    if component.url.contains('\'') || component.sha256.contains('\'') {
        return Err(Error::InvalidComponent(format!(
            "{:?} contains a quote",
            component.url
        )));
    }
    Ok(())
}

/// `license` MUST be a valid SPDX license expression, `version` a valid
/// semver version, and `platform` a valid target triple — shared by
/// [`import`] and [`import_downloaded`].
fn validate_metadata(license: &str, version: &str, platform: &str) -> Result<(), Error> {
    spdx::Expression::parse(license)
        .map_err(|error| Error::InvalidLicense(license.to_owned(), error))?;
    semver::Version::parse(version)
        .map_err(|error| Error::InvalidVersion(version.to_owned(), error))?;
    target_lexicon::Triple::from_str(platform)
        .map_err(|_error| Error::InvalidPlatform(platform.to_owned()))?;
    Ok(())
}

/// Write `src_dir`, if given, as a `RawTree` — shared by [`import`] and
/// [`import_downloaded`], since `src` is captured the same way regardless of
/// how `bin` is provisioned.
fn import_src(collector: &PackCollector, src_dir: Option<&Path>) -> Result<Option<RawTree>, Error> {
    src_dir
        .map(|dir| -> Result<RawTree, Error> {
            let tree = build_tree(collector, dir)?;
            Ok(RawTree::new(write_tree(collector, &tree)?))
        })
        .transpose()
}

/// Serialize `toolchain` and fast-forward `refs/meta/toolchains/<name>` to a
/// commit over it — the shared final step of [`import`] and
/// [`import_downloaded`].
fn store_toolchain(
    repo: &Path,
    name: &str,
    toolchain: Toolchain,
    odb: &gix::odb::Handle,
) -> Result<ObjectId, Error> {
    let oid = facet_git_tree::serialize_into(&toolchain, odb)?;
    let store = Store::open(repo)?;
    let message = match &toolchain.recipe {
        Some(recipe) => format!("git-toolchain: import {name} via {recipe}"),
        None => format!("git-toolchain: import {name}"),
    };
    store.store_tree(&toolchain_ref(name), oid, &message)?;
    Ok(oid)
}

/// The [`Toolchain`] document `refs/meta/toolchains/<name>`'s tip commit
/// holds.
pub fn resolve(repo: &Path, name: &str) -> Result<Toolchain, Error> {
    let store = Store::open(repo)?;
    let root = store.ref_tree(&toolchain_ref(name))?;
    let odb = odb_at(repo)?;
    Ok(facet_git_tree::deserialize(&root, &odb)?)
}

/// Every toolchain configured in `repo`, paired with its resolved document.
pub fn list(repo: &Path) -> Result<Vec<(String, Toolchain)>, Error> {
    let store = Store::open(repo)?;
    let odb = odb_at(repo)?;
    let prefix = format!("{TOOLCHAINS_NS}/");
    let mut out = Vec::new();
    for refname in store.list(&prefix)? {
        let Some(name) = refname.strip_prefix(&prefix) else {
            continue;
        };
        let tree = store.ref_tree(&refname)?;
        let toolchain = facet_git_tree::deserialize(&tree, &odb)?;
        out.push((name.to_owned(), toolchain));
    }
    Ok(out)
}

/// Toolchain `name`'s past imports, newest first, as `(committer unix
/// seconds, document)` pairs — one entry per commit on
/// `refs/meta/toolchains/<name>`, each document's own [`Toolchain::recipe`]
/// naming what produced it. The commit *is* the audit trail: no separate
/// provenance log is kept, since every [`import`]/[`import_downloaded`] call
/// already lands as a new commit on this ref.
pub fn history(repo: &Path, name: &str) -> Result<Vec<(u64, Toolchain)>, Error> {
    let store = Store::open(repo)?;
    Ok(store.history(&toolchain_ref(name))?)
}

/// Recreate the toolchain `name`'s `bin` (and `src`, if present) directory
/// under `dest`, restoring the executable bit and symlinks. Refuses to write
/// into a `dest` that already has contents. Returns the resolved document,
/// so the caller can report the license alongside the exported files.
///
/// [`Bin::Embedded`] writes its (already self-contained: executables plus a
/// sibling `lib/`) tree straight under `dest/bin`. [`Bin::Downloaded`]'s
/// components are each fetched, verified, and extracted per their own
/// recorded layout ([`Component::strip`], [`Component::dest`]) relative to
/// `dest`, landing at the same `dest/bin/…` shape by construction.
pub fn export(repo: &Path, name: &str, dest: &Path) -> Result<Toolchain, Error> {
    let toolchain = resolve(repo, name)?;
    let odb = odb_at(repo)?;
    ensure_empty_dest(dest)?;

    match &toolchain.bin {
        Bin::Embedded(tree) => {
            let bin_dest = dest.join("bin");
            fs::create_dir_all(&bin_dest).map_err(|error| Error::Io(bin_dest.clone(), error))?;
            write_tree_to_disk(&odb, tree.oid(), &bin_dest)?;
        }
        Bin::Downloaded(components) => download_components(components, dest)?,
    }

    if let Some(src) = &toolchain.src {
        let src_dest = dest.join("src");
        fs::create_dir_all(&src_dest).map_err(|error| Error::Io(src_dest.clone(), error))?;
        write_tree_to_disk(&odb, src.oid(), &src_dest)?;
    }
    Ok(toolchain)
}

/// Delete the toolchain `name`'s ref from `repo`.
pub fn remove(repo: &Path, name: &str) -> Result<(), Error> {
    let store = Store::open(repo)?;
    Ok(store.delete_ref(&toolchain_ref(name))?)
}

/// A toolchain's on-disk footprint, in bytes, summed from the git trees
/// backing it — `bin`'s tree when [`Bin::Embedded`] (`None` for
/// [`Bin::Downloaded`], since those bytes live at the distributor's archives,
/// not in this repository), and `src`'s tree, if imported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Facet)]
pub struct Usage {
    /// Total bytes across every blob in `bin`'s tree, or `None` when `bin` is
    /// [`Bin::Downloaded`] and so has no locally-stored tree to size.
    pub bin_bytes: Option<u64>,
    /// Total bytes across every blob in `src`'s tree, or `None` when no `src`
    /// was imported.
    pub src_bytes: Option<u64>,
}

/// Toolchain `name`'s on-disk footprint: recursively sums blob sizes in
/// `bin`'s tree (when [`Bin::Embedded`]) and `src`'s tree (if present), read
/// from object headers rather than fully decoding each blob.
pub fn disk_usage(repo: &Path, name: &str) -> Result<Usage, Error> {
    let toolchain = resolve(repo, name)?;
    let odb = odb_at(repo)?;
    let bin_bytes = match &toolchain.bin {
        Bin::Embedded(tree) => Some(tree_size(&odb, tree.oid())?),
        Bin::Downloaded(_) => None,
    };
    let src_bytes = toolchain
        .src
        .as_ref()
        .map(|tree| tree_size(&odb, tree.oid()))
        .transpose()?;
    Ok(Usage {
        bin_bytes,
        src_bytes,
    })
}

/// Recursively sum the byte size of every blob under `tree`, reading each
/// object's header (kind + size) rather than fully decoding its content —
/// cheap even for large binaries.
fn tree_size(odb: &gix::odb::Handle, tree: ObjectId) -> Result<u64, Error> {
    let mut buf = Vec::new();
    let tree_ref = odb
        .find_tree(&tree, &mut buf)
        .map_err(|error| git_store::Error::Object(error.to_string()))?;
    let mut total = 0u64;
    for entry in &tree_ref.entries {
        let size = match entry.mode.kind() {
            EntryKind::Tree => tree_size(odb, entry.oid.to_owned())?,
            EntryKind::Link | EntryKind::BlobExecutable | EntryKind::Blob => odb
                .header(entry.oid)
                .map_err(|error| git_store::Error::Object(error.to_string()))?
                .size(),
            // A submodule gitlink: no blob of its own to size in this repo.
            EntryKind::Commit => 0,
        };
        total = total.saturating_add(size);
    }
    Ok(total)
}

/// `refs/meta/toolchains/<name>`.
fn toolchain_ref(name: &str) -> String {
    format!("{TOOLCHAINS_NS}/{name}")
}

/// Open a raw object database on `repo`'s common git directory — the same
/// object IO [`git_store::Store`] uses internally, opened again here since
/// walking a directory into a tree (unlike a `Facet` document) is this
/// crate's own concern rather than something `Store` exposes plumbing for
/// beyond the finished tree's commit and ref.
fn odb_at(repo: &Path) -> Result<gix::odb::Handle, Error> {
    let opened = gix::open(repo).map_err(|error| git_store::Error::Open(Box::new(error)))?;
    Ok(gix::odb::at(opened.common_dir().join("objects")).map_err(|_io| git_store::Error::Odb)?)
}

/// Accumulates every blob and tree object produced while walking a directory
/// into a [`Tree`], deferring the actual object-database write until the
/// whole walk is done so the result can land as a single pack (see
/// [`PackCollector::flush`]) instead of one loose object per file.
///
/// Cheaply `Clone` (an `Arc`-backed handle to the same map), the same way
/// `gix::odb::Handle` was before this replaced it — each parallel work item
/// in [`build_tree`] gets its own clone.
type CollectedObjects = HashMap<ObjectId, (gix::objs::Kind, Vec<u8>)>;

#[derive(Clone, Default)]
struct PackCollector(std::sync::Arc<Mutex<CollectedObjects>>);

impl PackCollector {
    /// Hash `data` and record it under its own object id, unless an object
    /// with that id is already recorded — the same "already exists, skip
    /// the write" behavior loose object writes have implicitly, since
    /// identical file contents (e.g. duplicate license files) hash to the
    /// same oid.
    fn insert(&self, kind: gix::objs::Kind, data: Vec<u8>) -> Result<ObjectId, Error> {
        let oid = gix::objs::compute_hash(gix::hash::Kind::Sha1, kind, &data)
            .map_err(|error| git_store::Error::Object(error.to_string()))?;
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(oid)
            .or_insert((kind, data));
        Ok(oid)
    }

    /// Write every collected object into `repo`'s object database as a
    /// single pack: one sequential data-file write plus one index build,
    /// rather than the tempfile-create, fan-out-directory-check, and rename
    /// every loose object write pays individually (see `build_tree`'s doc
    /// comment on why that per-object cost dominates a large import).
    fn flush(self, repo: &Path) -> Result<(), Error> {
        let objects = std::mem::take(&mut *self.0.lock().unwrap_or_else(PoisonError::into_inner));
        if objects.is_empty() {
            return Ok(());
        }

        let hash = gix::hash::Kind::Sha1;
        let mut offset = 0u64;
        let mut entries = Vec::with_capacity(objects.len());
        for (kind, data) in objects.into_values() {
            let object = gix::objs::Data {
                kind,
                object_hash: hash,
                data: &data,
            };
            let entry = PackEntry::from_data_obj(&object, offset)
                .map_err(|error| git_store::Error::Object(error.to_string()))?;
            offset = offset
                .checked_add(entry.bytes_in_pack())
                .ok_or_else(|| git_store::Error::Object("pack too large".to_owned()))?;
            entries.push(entry);
        }

        let pack_file = tempfile::tempfile().map_err(|error| Error::Io(repo.to_owned(), error))?;
        let writer = gix_pack::data::input::EntriesToBytesIter::new(
            entries
                .into_iter()
                .map(Ok::<_, gix_pack::data::input::Error>),
            &pack_file,
            gix_pack::data::Version::V2,
            hash,
        );
        for entry in writer {
            entry.map_err(|error| git_store::Error::Object(error.to_string()))?;
        }
        (&pack_file)
            .seek(SeekFrom::Start(0))
            .map_err(|error| Error::Io(repo.to_owned(), error))?;

        let opened = gix::open(repo).map_err(|error| git_store::Error::Open(Box::new(error)))?;
        let pack_dir = opened.common_dir().join("objects").join("pack");
        fs::create_dir_all(&pack_dir).map_err(|error| Error::Io(pack_dir.clone(), error))?;

        let outcome = gix_pack::Bundle::write_to_directory(
            &mut std::io::BufReader::new(&pack_file),
            Some(&pack_dir),
            &mut gix::progress::Discard,
            &AtomicBool::new(false),
            None::<gix::odb::Handle>,
            gix_pack::bundle::write::Options {
                object_hash: hash,
                ..Default::default()
            },
        )
        .map_err(|error| git_store::Error::Object(error.to_string()))?;

        if let Some(keep_path) = outcome.keep_path {
            fs::remove_file(&keep_path).map_err(|error| Error::Io(keep_path, error))?;
        }
        Ok(())
    }
}

fn write_tree(collector: &PackCollector, tree: &Tree) -> Result<ObjectId, Error> {
    let mut buf = Vec::with_capacity(2048);
    tree.write_to(&mut buf)
        .map_err(|error| git_store::Error::Object(error.to_string()))?;
    collector.insert(gix::objs::Kind::Tree, buf)
}

/// Build `dir`'s tree bottom-up: a directory's own entries are all resolved
/// in parallel (recursing into subdirectories, writing files and symlinks as
/// blobs) before its own tree object is written, so every child is already
/// an object id by the time its parent's entry list is sorted and written.
///
/// A large import (a rustup sysroot's `lib/rustlib/src/rust` alone is tens
/// of thousands of files) is dominated by per-object filesystem syscall
/// overhead, not by hashing or compression (gix's loose-object writer already
/// runs zlib at its fastest level); fanning the write out across every core
/// is the lever that actually matters here. Object bytes are hashed and
/// accumulated in a [`PackCollector`] rather than written to the object
/// database as they're produced — [`PackCollector::flush`] writes them all
/// as a single pack once the whole walk is done, since even parallel loose
/// writes are still one tempfile-create/fan-out-check/rename apiece.
fn build_tree(collector: &PackCollector, dir: &Path) -> Result<Tree, Error> {
    let read_dir = fs::read_dir(dir).map_err(|error| Error::Io(dir.to_owned(), error))?;
    let items = read_dir
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| Error::Io(dir.to_owned(), error))?;

    let mut entries: Vec<TreeEntry> = items
        .into_iter()
        .map(|item| (item, collector.clone()))
        .collect::<Vec<_>>()
        .into_par_iter()
        .map(|(item, collector)| -> Result<Option<TreeEntry>, Error> {
            let path = item.path();
            let name = item
                .file_name()
                .into_string()
                .map_err(|_name| Error::NotUtf8(path.clone()))?;
            let file_type = item
                .file_type()
                .map_err(|error| Error::Io(path.clone(), error))?;
            let Some((oid, mode)) = write_entry(&collector, &path, file_type)? else {
                return Ok(None);
            };
            Ok(Some(TreeEntry {
                mode,
                filename: name.into(),
                oid,
            }))
        })
        .collect::<Result<Vec<Option<TreeEntry>>, Error>>()?
        .into_iter()
        .flatten()
        .collect();
    entries.sort();
    Ok(Tree { entries })
}

/// Write one directory entry to the object database, or `None` for an empty
/// subdirectory — unrepresentable in a git tree, so skipped rather than
/// written as a bare tree object.
fn write_entry(
    collector: &PackCollector,
    path: &Path,
    file_type: fs::FileType,
) -> Result<Option<(ObjectId, EntryMode)>, Error> {
    if file_type.is_dir() {
        let tree = build_tree(collector, path)?;
        if tree.entries.is_empty() {
            return Ok(None);
        }
        let oid = write_tree(collector, &tree)?;
        return Ok(Some((oid, EntryMode::from(EntryKind::Tree))));
    }
    if file_type.is_symlink() {
        let target = fs::read_link(path).map_err(|error| Error::Io(path.to_owned(), error))?;
        let target = target
            .to_str()
            .ok_or_else(|| Error::NotUtf8(path.to_owned()))?;
        let oid = collector.insert(gix::objs::Kind::Blob, target.as_bytes().to_vec())?;
        return Ok(Some((oid, EntryMode::from(EntryKind::Link))));
    }
    let bytes = fs::read(path).map_err(|error| Error::Io(path.to_owned(), error))?;
    let executable = fs::metadata(path)
        .map_err(|error| Error::Io(path.to_owned(), error))?
        .permissions()
        .mode()
        & 0o111
        != 0;
    let oid = collector.insert(gix::objs::Kind::Blob, bytes)?;
    let kind = if executable {
        EntryKind::BlobExecutable
    } else {
        EntryKind::Blob
    };
    Ok(Some((oid, EntryMode::from(kind))))
}

fn ensure_empty_dest(dest: &Path) -> Result<(), Error> {
    if dest.exists() {
        let mut entries = fs::read_dir(dest).map_err(|error| Error::Io(dest.to_owned(), error))?;
        if entries.next().is_some() {
            return Err(Error::DestNotEmpty(dest.to_owned()));
        }
    } else {
        fs::create_dir_all(dest).map_err(|error| Error::Io(dest.to_owned(), error))?;
    }
    Ok(())
}

/// Walk `tree` back onto disk under `dest`, recursing into subdirectories
/// before returning — the export side of [`build_tree`].
fn write_tree_to_disk(odb: &gix::odb::Handle, tree: ObjectId, dest: &Path) -> Result<(), Error> {
    let mut buf = Vec::new();
    let tree_ref = odb
        .find_tree(&tree, &mut buf)
        .map_err(|error| git_store::Error::Object(error.to_string()))?;
    for entry in &tree_ref.entries {
        let name = entry
            .filename
            .to_str()
            .map_err(|_error| Error::NotUtf8(dest.to_owned()))?;
        let path = dest.join(name);
        match entry.mode.kind() {
            EntryKind::Tree => {
                fs::create_dir_all(&path).map_err(|error| Error::Io(path.clone(), error))?;
                write_tree_to_disk(odb, entry.oid.to_owned(), &path)?;
            }
            EntryKind::Link => {
                let mut blob_buf = Vec::new();
                let blob = odb
                    .find_blob(entry.oid, &mut blob_buf)
                    .map_err(|error| git_store::Error::Object(error.to_string()))?;
                let target = blob
                    .data
                    .to_str()
                    .map_err(|_error| Error::NotUtf8(path.clone()))?;
                std::os::unix::fs::symlink(target, &path)
                    .map_err(|error| Error::Io(path.clone(), error))?;
            }
            EntryKind::BlobExecutable | EntryKind::Blob => {
                let mut blob_buf = Vec::new();
                let blob = odb
                    .find_blob(entry.oid, &mut blob_buf)
                    .map_err(|error| git_store::Error::Object(error.to_string()))?;
                fs::write(&path, blob.data).map_err(|error| Error::Io(path.clone(), error))?;
                if entry.mode.is_executable() {
                    let mut perms = fs::metadata(&path)
                        .map_err(|error| Error::Io(path.clone(), error))?
                        .permissions();
                    perms.set_mode(0o755);
                    fs::set_permissions(&path, perms)
                        .map_err(|error| Error::Io(path.clone(), error))?;
                }
            }
            EntryKind::Commit => {
                // A git submodule commit reference; nothing to write here.
            }
        }
    }
    Ok(())
}

/// Fetch, verify, and extract every component of a [`Bin::Downloaded`]
/// toolchain into `dest`, in order — later components overlay earlier ones,
/// matching how rustup itself layers `rustc`/`cargo`/`rust-std` onto one
/// sysroot.
fn download_components(components: &[Component], dest: &Path) -> Result<(), Error> {
    for component in components {
        let archive = fetch(&component.url)?;
        let actual = sha256_hex(&archive)?;
        if actual != component.sha256 {
            return Err(Error::HashMismatch(
                component.url.clone(),
                component.sha256.clone(),
                actual,
            ));
        }
        extract_component(&archive, component, dest)?;
    }
    Ok(())
}

/// `GET url` via the system `curl`, returning the response body — shells out
/// rather than adding an HTTP client dependency to this crate.
fn fetch(url: &str) -> Result<Vec<u8>, Error> {
    let output = Command::new("curl")
        .args(["-fsSL", url])
        .output()
        .map_err(|error| Error::Fetch(url.to_owned(), error.to_string()))?;
    if !output.status.success() {
        return Err(Error::Fetch(
            url.to_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ));
    }
    Ok(output.stdout)
}

/// Hex-encoded sha256 of `bytes`, via the system `shasum` (macOS) or
/// `sha256sum` (Linux) — shells out rather than adding a hashing dependency
/// to this crate. Public so a recipe pinning a hosted archive (trust on
/// first use) computes its hash the same way every later verification does.
pub fn sha256_hex(bytes: &[u8]) -> Result<String, Error> {
    let (program, args): (&str, &[&str]) = match std::env::consts::OS {
        "macos" => ("shasum", &["-a", "256"]),
        _ => ("sha256sum", &[]),
    };
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| Error::Fetch(program.to_owned(), error.to_string()))?;
    child
        .stdin
        .take()
        .ok_or_else(|| Error::Fetch(program.to_owned(), "no stdin".to_owned()))?
        .write_all(bytes)
        .map_err(|error| Error::Fetch(program.to_owned(), error.to_string()))?;
    let output = child
        .wait_with_output()
        .map_err(|error| Error::Fetch(program.to_owned(), error.to_string()))?;
    if !output.status.success() {
        return Err(Error::Fetch(
            program.to_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ));
    }
    let hex = String::from_utf8_lossy(&output.stdout);
    hex.split_whitespace()
        .next()
        .map(str::to_owned)
        .ok_or_else(|| Error::Fetch(program.to_owned(), "no hash in output".to_owned()))
}

/// Extract a tar `archive` (compression auto-detected — gzip, xz, ...) per
/// `component`'s recorded layout: strip its leading [`Component::strip`]
/// path segments and land it in `dest_root`/[`Component::dest`].
fn extract_component(archive: &[u8], component: &Component, dest_root: &Path) -> Result<(), Error> {
    let dest = if component.dest.is_empty() {
        dest_root.to_owned()
    } else {
        dest_root.join(&component.dest)
    };
    fs::create_dir_all(&dest).map_err(|error| Error::Io(dest.clone(), error))?;
    let strip = format!("--strip-components={}", component.strip);
    let mut child = Command::new("tar")
        .args(["-x", &strip, "-C"])
        .arg(&dest)
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|error| Error::Fetch("tar".to_owned(), error.to_string()))?;
    child
        .stdin
        .take()
        .ok_or_else(|| Error::Fetch("tar".to_owned(), "no stdin".to_owned()))?
        .write_all(archive)
        .map_err(|error| Error::Fetch("tar".to_owned(), error.to_string()))?;
    let status = child
        .wait()
        .map_err(|error| Error::Fetch("tar".to_owned(), error.to_string()))?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::Fetch(
            "tar".to_owned(),
            "extraction failed".to_owned(),
        ))
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::indexing_slicing,
        clippy::unreachable,
        reason = "unit test"
    )]

    use git_store::test_support::repo;

    use super::*;

    /// A valid semver version and target triple, reused across tests that
    /// only care about `license`.
    const VERSION: &str = "1.0.0";
    const PLATFORM: &str = "x86_64-unknown-linux-gnu";

    /// A file, an executable, and (on unix) a symlink, plus an empty
    /// subdirectory — enough to exercise every branch of `write_entry`.
    fn populate(dir: &Path) {
        fs::write(dir.join("README"), b"hello\n").unwrap();
        fs::write(dir.join("tool"), b"#!/bin/sh\necho hi\n").unwrap();
        let mut perms = fs::metadata(dir.join("tool")).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(dir.join("tool"), perms).unwrap();
        std::os::unix::fs::symlink("tool", dir.join("tool-link")).unwrap();
        fs::create_dir(dir.join("empty")).unwrap();
    }

    #[test]
    fn import_is_deterministic_across_two_directories() {
        let repo_dir = repo();
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        populate(a.path());
        populate(b.path());

        let first = import(
            repo_dir.path(),
            "gcc",
            a.path(),
            None,
            "MIT",
            VERSION,
            PLATFORM,
            None,
        )
        .unwrap();
        let second = import(
            repo_dir.path(),
            "clang",
            b.path(),
            None,
            "MIT",
            VERSION,
            PLATFORM,
            None,
        )
        .unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn import_skips_empty_directories() {
        let repo_dir = repo();
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());

        import(
            repo_dir.path(),
            "gcc",
            dir.path(),
            None,
            "MIT",
            VERSION,
            PLATFORM,
            None,
        )
        .unwrap();
        let toolchain = resolve(repo_dir.path(), "gcc").unwrap();
        let Bin::Embedded(bin) = &toolchain.bin else {
            unreachable!("import always produces an embedded bin");
        };
        let odb = odb_at(repo_dir.path()).unwrap();
        let mut buf = Vec::new();
        let tree = odb.find_tree(&bin.oid(), &mut buf).unwrap();
        assert!(tree.entries.iter().all(|entry| entry.filename != "empty"));
    }

    #[test]
    fn import_then_resolve_round_trips() {
        let repo_dir = repo();
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());

        let oid = import(
            repo_dir.path(),
            "gcc",
            dir.path(),
            None,
            "MIT",
            VERSION,
            PLATFORM,
            None,
        )
        .unwrap();
        let toolchain = resolve(repo_dir.path(), "gcc").unwrap();
        assert_eq!(toolchain.license, "MIT");
        assert!(toolchain.src.is_none());

        let odb = odb_at(repo_dir.path()).unwrap();
        assert_eq!(
            facet_git_tree::serialize_into(&toolchain, &odb).unwrap(),
            oid
        );
    }

    #[test]
    fn import_then_export_round_trips_contents_and_exec_bit() {
        let repo_dir = repo();
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        import(
            repo_dir.path(),
            "gcc",
            dir.path(),
            None,
            "MIT",
            VERSION,
            PLATFORM,
            None,
        )
        .unwrap();

        let dest = tempfile::tempdir().unwrap();
        let dest_path = dest.path().join("out");
        let toolchain = export(repo_dir.path(), "gcc", &dest_path).unwrap();
        assert_eq!(toolchain.license, "MIT");

        assert_eq!(fs::read(dest_path.join("bin/README")).unwrap(), b"hello\n");
        let tool_perms = fs::metadata(dest_path.join("bin/tool"))
            .unwrap()
            .permissions();
        assert_eq!(tool_perms.mode() & 0o111, 0o111);
        let link_target = fs::read_link(dest_path.join("bin/tool-link")).unwrap();
        assert_eq!(link_target, Path::new("tool"));
        assert!(!dest_path.join("bin/empty").exists());
        assert!(!dest_path.join("src").exists());
    }

    #[test]
    fn import_then_export_round_trips_src_too() {
        let repo_dir = repo();
        let bin_dir = tempfile::tempdir().unwrap();
        let src_dir = tempfile::tempdir().unwrap();
        populate(bin_dir.path());
        fs::write(src_dir.path().join("main.c"), b"int main() {}\n").unwrap();
        import(
            repo_dir.path(),
            "gcc",
            bin_dir.path(),
            Some(src_dir.path()),
            "MIT",
            VERSION,
            PLATFORM,
            None,
        )
        .unwrap();

        let toolchain = resolve(repo_dir.path(), "gcc").unwrap();
        assert!(toolchain.src.is_some());

        let dest = tempfile::tempdir().unwrap();
        let dest_path = dest.path().join("out");
        export(repo_dir.path(), "gcc", &dest_path).unwrap();
        assert_eq!(
            fs::read(dest_path.join("src/main.c")).unwrap(),
            b"int main() {}\n"
        );
        assert_eq!(fs::read(dest_path.join("bin/README")).unwrap(), b"hello\n");
    }

    /// Build a `file://` component archive matching real dist tarballs'
    /// layout (`<pkg>-<version>-<target>/<component>/<payload>`), returning
    /// its `file://` URL and sha256, ready to hand to [`Component`].
    fn build_component(staging: &Path, payload: &[(&str, &[u8])]) -> Component {
        let root = staging.join("pkg-1.0.0-target/component");
        for (path, contents) in payload {
            let full = root.join(path);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(&full, contents).unwrap();
        }
        let archive = staging.join("component.tar.gz");
        let status = Command::new("tar")
            .args(["-czf"])
            .arg(&archive)
            .args(["-C"])
            .arg(staging)
            .arg("pkg-1.0.0-target/component")
            .status()
            .unwrap();
        assert!(status.success());
        let bytes = fs::read(&archive).unwrap();
        Component {
            url: format!("file://{}", archive.display()),
            sha256: sha256_hex(&bytes).unwrap(),
            strip: 2,
            dest: String::new(),
        }
    }

    /// Build a flat single-binary `.tar.xz` release archive
    /// (`<pkg>-<version>/<binary>`), the shape a GitHub release or a zig
    /// tarball ships — one leading segment, no `bin/` of its own.
    fn build_flat_component(staging: &Path, name: &str, contents: &[u8]) -> Component {
        let root = staging.join("pkg-1.0.0");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(name), contents).unwrap();
        let archive = staging.join("flat.tar.xz");
        let status = Command::new("tar")
            .args(["-cJf"])
            .arg(&archive)
            .args(["-C"])
            .arg(staging)
            .arg("pkg-1.0.0")
            .status()
            .unwrap();
        assert!(status.success());
        let bytes = fs::read(&archive).unwrap();
        Component {
            url: format!("file://{}", archive.display()),
            sha256: sha256_hex(&bytes).unwrap(),
            strip: 1,
            dest: "bin".to_owned(),
        }
    }

    #[test]
    fn import_downloaded_then_export_extracts_and_strips_components() {
        let repo_dir = repo();
        let staging = tempfile::tempdir().unwrap();
        let component = build_component(staging.path(), &[("bin/tool", b"#!/bin/sh\n")]);

        import_downloaded(
            repo_dir.path(),
            "rustup-like",
            vec![component],
            None,
            "MIT",
            VERSION,
            PLATFORM,
            None,
        )
        .unwrap();

        let dest = tempfile::tempdir().unwrap();
        let dest_path = dest.path().join("out");
        let toolchain = export(repo_dir.path(), "rustup-like", &dest_path).unwrap();
        assert!(matches!(toolchain.bin, Bin::Downloaded(_)));
        assert_eq!(
            fs::read(dest_path.join("bin/tool")).unwrap(),
            b"#!/bin/sh\n"
        );
    }

    #[test]
    fn export_extracts_a_flat_xz_component_into_its_dest() {
        let repo_dir = repo();
        let staging = tempfile::tempdir().unwrap();
        let component = build_flat_component(staging.path(), "tool", b"#!/bin/sh\n");

        import_downloaded(
            repo_dir.path(),
            "flat",
            vec![component],
            None,
            "MIT",
            VERSION,
            PLATFORM,
            None,
        )
        .unwrap();

        let dest = tempfile::tempdir().unwrap();
        let dest_path = dest.path().join("out");
        export(repo_dir.path(), "flat", &dest_path).unwrap();
        assert_eq!(
            fs::read(dest_path.join("bin/tool")).unwrap(),
            b"#!/bin/sh\n"
        );
    }

    #[test]
    fn import_downloaded_rejects_an_unsafe_component() {
        let repo_dir = repo();
        let staging = tempfile::tempdir().unwrap();
        let mut component = build_component(staging.path(), &[("bin/tool", b"#!/bin/sh\n")]);
        component.dest = "a/b".to_owned();
        let result = import_downloaded(
            repo_dir.path(),
            "bad-dest",
            vec![component],
            None,
            "MIT",
            VERSION,
            PLATFORM,
            None,
        );
        assert!(matches!(result, Err(Error::InvalidComponent(_))));

        let mut component = build_component(staging.path(), &[("bin/tool", b"#!/bin/sh\n")]);
        component.url = "https://example.com/x' rm -rf'".to_owned();
        let result = import_downloaded(
            repo_dir.path(),
            "bad-url",
            vec![component],
            None,
            "MIT",
            VERSION,
            PLATFORM,
            None,
        );
        assert!(matches!(result, Err(Error::InvalidComponent(_))));
    }

    #[test]
    fn import_downloaded_rejects_an_empty_component_list() {
        let repo_dir = repo();
        let result = import_downloaded(
            repo_dir.path(),
            "rustup-like",
            vec![],
            None,
            "MIT",
            VERSION,
            PLATFORM,
            None,
        );
        assert!(matches!(result, Err(Error::NoComponents)));
    }

    #[test]
    fn export_rejects_a_component_whose_hash_does_not_match() {
        let repo_dir = repo();
        let staging = tempfile::tempdir().unwrap();
        let mut component = build_component(staging.path(), &[("bin/tool", b"#!/bin/sh\n")]);
        component.sha256 = "0".repeat(64);

        import_downloaded(
            repo_dir.path(),
            "rustup-like",
            vec![component],
            None,
            "MIT",
            VERSION,
            PLATFORM,
            None,
        )
        .unwrap();

        let dest = tempfile::tempdir().unwrap();
        let dest_path = dest.path().join("out");
        let result = export(repo_dir.path(), "rustup-like", &dest_path);
        assert!(matches!(result, Err(Error::HashMismatch(_, _, _))));
    }

    #[test]
    fn export_refuses_a_non_empty_destination() {
        let repo_dir = repo();
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        import(
            repo_dir.path(),
            "gcc",
            dir.path(),
            None,
            "MIT",
            VERSION,
            PLATFORM,
            None,
        )
        .unwrap();

        let dest = tempfile::tempdir().unwrap();
        fs::write(dest.path().join("already-here"), b"x").unwrap();
        let result = export(repo_dir.path(), "gcc", dest.path());
        assert!(matches!(result, Err(Error::DestNotEmpty(_))));
    }

    #[test]
    fn import_rejects_an_invalid_license() {
        let repo_dir = repo();
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        let result = import(
            repo_dir.path(),
            "gcc",
            dir.path(),
            None,
            "not a license",
            VERSION,
            PLATFORM,
            None,
        );
        assert!(matches!(result, Err(Error::InvalidLicense(_, _))));
    }

    #[test]
    fn import_rejects_an_invalid_version() {
        let repo_dir = repo();
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        let result = import(
            repo_dir.path(),
            "gcc",
            dir.path(),
            None,
            "MIT",
            "not-semver",
            PLATFORM,
            None,
        );
        assert!(matches!(result, Err(Error::InvalidVersion(_, _))));
    }

    #[test]
    fn import_rejects_an_invalid_platform() {
        let repo_dir = repo();
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        let result = import(
            repo_dir.path(),
            "gcc",
            dir.path(),
            None,
            "MIT",
            VERSION,
            "not a platform!!",
            None,
        );
        assert!(matches!(result, Err(Error::InvalidPlatform(_))));
    }

    #[test]
    fn import_rejects_an_empty_bin_directory() {
        let repo_dir = repo();
        let dir = tempfile::tempdir().unwrap();
        // Only an empty subdirectory: `build_tree` skips it, so `bin` ends up
        // with nothing importable.
        fs::create_dir(dir.path().join("empty")).unwrap();
        let result = import(
            repo_dir.path(),
            "gcc",
            dir.path(),
            None,
            "MIT",
            VERSION,
            PLATFORM,
            None,
        );
        assert!(matches!(result, Err(Error::EmptyBin(_))));
    }

    #[test]
    fn list_returns_every_toolchain_with_its_document() {
        let repo_dir = repo();
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        populate(a.path());
        fs::write(b.path().join("distinct"), b"x").unwrap();

        import(
            repo_dir.path(),
            "gcc",
            a.path(),
            None,
            "MIT",
            VERSION,
            PLATFORM,
            None,
        )
        .unwrap();
        import(
            repo_dir.path(),
            "clang",
            b.path(),
            None,
            "Apache-2.0",
            VERSION,
            PLATFORM,
            None,
        )
        .unwrap();

        let mut listed = list(repo_dir.path()).unwrap();
        listed.sort_by(|(a, _), (b, _)| a.cmp(b));
        let names: Vec<&str> = listed.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, vec!["clang", "gcc"]);
        assert_eq!(listed[0].1.license, "Apache-2.0");
        assert_eq!(listed[1].1.license, "MIT");
    }

    #[test]
    fn remove_deletes_the_ref() {
        let repo_dir = repo();
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        import(
            repo_dir.path(),
            "gcc",
            dir.path(),
            None,
            "MIT",
            VERSION,
            PLATFORM,
            None,
        )
        .unwrap();

        remove(repo_dir.path(), "gcc").unwrap();
        let _ = resolve(repo_dir.path(), "gcc").unwrap_err();
    }

    #[test]
    fn import_rejects_an_invalid_name() {
        let repo_dir = repo();
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        let result = import(
            repo_dir.path(),
            "not/valid",
            dir.path(),
            None,
            "MIT",
            VERSION,
            PLATFORM,
            None,
        );
        assert!(matches!(result, Err(Error::InvalidName(_))));
    }
}
