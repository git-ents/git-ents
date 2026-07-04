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

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use facet::Facet;
use facet_git_tree::RawTree;
use git_store::Store;
use gix::ObjectId;
use gix::bstr::ByteSlice as _;
use gix::objs::tree::{Entry as TreeEntry, EntryKind, EntryMode};
use gix::objs::{FindExt as _, Tree, Write as _};

/// The ref namespace holding toolchains, one ref per toolchain:
/// `refs/meta/toolchains/<name>`. A toolchain's identity is its tip commit's
/// tree hash, so importing identical contents twice is a no-op churn-wise.
pub const TOOLCHAINS_NS: &str = "refs/meta/toolchains";

/// A toolchain: an executable `bin` directory, an optional `src` directory,
/// and the license covering them — the document stored at the tip of
/// `refs/meta/toolchains/<name>`.
///
/// `bin` and `src` are [`RawTree`]: each is captured as a single opaque git
/// tree by [`import`], not walked field-by-field, since a toolchain's
/// on-disk layout has no fixed shape for `Facet` to model.
#[derive(Debug, Clone, PartialEq, Facet)]
pub struct Toolchain {
    /// The toolchain's executables, activated on `PATH` when a check
    /// requests it.
    pub bin: RawTree,
    /// The toolchain's source, if imported — not activated on `PATH`, kept
    /// only for provenance.
    pub src: Option<RawTree>,
    /// The license covering `bin` (and `src`, if present).
    pub license: String,
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
    /// [`import`]'s `license` argument was empty.
    #[error("toolchain license must not be empty")]
    EmptyLicense,
}

/// Import `bin_dir` (and, optionally, `src_dir`) into `repo` as the
/// toolchain `name`: write each directory tree bottom-up into the object
/// database, assemble a [`Toolchain`] document over them and `license`, and
/// fast-forward `refs/meta/toolchains/<name>` to a commit over it. Returns
/// the document's root tree object id.
pub fn import(
    repo: &Path,
    name: &str,
    bin_dir: &Path,
    src_dir: Option<&Path>,
    license: &str,
) -> Result<ObjectId, Error> {
    if !git_store::ref_segment_ok(name) {
        return Err(Error::InvalidName(name.to_owned()));
    }
    if license.is_empty() {
        return Err(Error::EmptyLicense);
    }
    let odb = odb_at(repo)?;

    let bin_tree = build_tree(&odb, bin_dir)?;
    if bin_tree.entries.is_empty() {
        return Err(Error::EmptyBin(bin_dir.to_owned()));
    }
    let bin = RawTree::new(write_object(&odb, &bin_tree)?);

    let src = src_dir
        .map(|dir| -> Result<RawTree, Error> {
            let tree = build_tree(&odb, dir)?;
            Ok(RawTree::new(write_object(&odb, &tree)?))
        })
        .transpose()?;

    let toolchain = Toolchain {
        bin,
        src,
        license: license.to_owned(),
    };
    let oid = facet_git_tree::serialize_into(&toolchain, &odb)?;

    let store = Store::open(repo)?;
    store.store_tree(
        &toolchain_ref(name),
        oid,
        &format!("git-toolchain: import {name}"),
    )?;
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

/// Recreate the toolchain `name`'s `bin` (and `src`, if present) directory
/// under `dest`, restoring the executable bit and symlinks. Refuses to write
/// into a `dest` that already has contents. Returns the resolved document,
/// so the caller can report the license alongside the exported files.
pub fn export(repo: &Path, name: &str, dest: &Path) -> Result<Toolchain, Error> {
    let toolchain = resolve(repo, name)?;
    let odb = odb_at(repo)?;
    ensure_empty_dest(dest)?;

    let bin_dest = dest.join("bin");
    fs::create_dir_all(&bin_dest).map_err(|error| Error::Io(bin_dest.clone(), error))?;
    write_tree_to_disk(&odb, toolchain.bin.oid(), &bin_dest)?;

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

fn write_object(odb: &gix::odb::Handle, tree: &Tree) -> Result<ObjectId, Error> {
    Ok(odb
        .write(tree)
        .map_err(|error| git_store::Error::Object(error.to_string()))?)
}

/// Build `dir`'s tree bottom-up: a directory's own entries are all resolved
/// (recursing into subdirectories, writing files and symlinks as blobs)
/// before its own tree object is written, so every child is already an
/// object id by the time its parent's entry list is sorted and written.
fn build_tree(odb: &gix::odb::Handle, dir: &Path) -> Result<Tree, Error> {
    let mut entries = Vec::new();
    let read_dir = fs::read_dir(dir).map_err(|error| Error::Io(dir.to_owned(), error))?;
    for item in read_dir {
        let item = item.map_err(|error| Error::Io(dir.to_owned(), error))?;
        let path = item.path();
        let name = item
            .file_name()
            .into_string()
            .map_err(|_name| Error::NotUtf8(path.clone()))?;
        let file_type = item
            .file_type()
            .map_err(|error| Error::Io(path.clone(), error))?;
        let Some((oid, mode)) = write_entry(odb, &path, file_type)? else {
            continue;
        };
        entries.push(TreeEntry {
            mode,
            filename: name.into(),
            oid,
        });
    }
    entries.sort();
    Ok(Tree { entries })
}

/// Write one directory entry to the object database, or `None` for an empty
/// subdirectory — unrepresentable in a git tree, so skipped rather than
/// written as a bare tree object.
fn write_entry(
    odb: &gix::odb::Handle,
    path: &Path,
    file_type: fs::FileType,
) -> Result<Option<(ObjectId, EntryMode)>, Error> {
    if file_type.is_dir() {
        let tree = build_tree(odb, path)?;
        if tree.entries.is_empty() {
            return Ok(None);
        }
        let oid = write_object(odb, &tree)?;
        return Ok(Some((oid, EntryMode::from(EntryKind::Tree))));
    }
    if file_type.is_symlink() {
        let target = fs::read_link(path).map_err(|error| Error::Io(path.to_owned(), error))?;
        let target = target
            .to_str()
            .ok_or_else(|| Error::NotUtf8(path.to_owned()))?;
        let oid = odb
            .write_buf(gix::objs::Kind::Blob, target.as_bytes())
            .map_err(|error| git_store::Error::Object(error.to_string()))?;
        return Ok(Some((oid, EntryMode::from(EntryKind::Link))));
    }
    let bytes = fs::read(path).map_err(|error| Error::Io(path.to_owned(), error))?;
    let executable = fs::metadata(path)
        .map_err(|error| Error::Io(path.to_owned(), error))?
        .permissions()
        .mode()
        & 0o111
        != 0;
    let oid = odb
        .write_buf(gix::objs::Kind::Blob, &bytes)
        .map_err(|error| git_store::Error::Object(error.to_string()))?;
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, reason = "unit test")]

    use git_store::test_support::repo;

    use super::*;

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

        let first = import(repo_dir.path(), "gcc", a.path(), None, "MIT").unwrap();
        let second = import(repo_dir.path(), "clang", b.path(), None, "MIT").unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn import_skips_empty_directories() {
        let repo_dir = repo();
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());

        import(repo_dir.path(), "gcc", dir.path(), None, "MIT").unwrap();
        let toolchain = resolve(repo_dir.path(), "gcc").unwrap();
        let odb = odb_at(repo_dir.path()).unwrap();
        let mut buf = Vec::new();
        let tree = odb.find_tree(&toolchain.bin.oid(), &mut buf).unwrap();
        assert!(tree.entries.iter().all(|entry| entry.filename != "empty"));
    }

    #[test]
    fn import_then_resolve_round_trips() {
        let repo_dir = repo();
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());

        let oid = import(repo_dir.path(), "gcc", dir.path(), None, "MIT").unwrap();
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
        import(repo_dir.path(), "gcc", dir.path(), None, "MIT").unwrap();

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

    #[test]
    fn export_refuses_a_non_empty_destination() {
        let repo_dir = repo();
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        import(repo_dir.path(), "gcc", dir.path(), None, "MIT").unwrap();

        let dest = tempfile::tempdir().unwrap();
        fs::write(dest.path().join("already-here"), b"x").unwrap();
        let result = export(repo_dir.path(), "gcc", dest.path());
        assert!(matches!(result, Err(Error::DestNotEmpty(_))));
    }

    #[test]
    fn import_rejects_an_empty_license() {
        let repo_dir = repo();
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        let result = import(repo_dir.path(), "gcc", dir.path(), None, "");
        assert!(matches!(result, Err(Error::EmptyLicense)));
    }

    #[test]
    fn import_rejects_an_empty_bin_directory() {
        let repo_dir = repo();
        let dir = tempfile::tempdir().unwrap();
        // Only an empty subdirectory: `build_tree` skips it, so `bin` ends up
        // with nothing importable.
        fs::create_dir(dir.path().join("empty")).unwrap();
        let result = import(repo_dir.path(), "gcc", dir.path(), None, "MIT");
        assert!(matches!(result, Err(Error::EmptyBin(_))));
    }

    #[test]
    fn list_returns_every_toolchain_with_its_document() {
        let repo_dir = repo();
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        populate(a.path());
        fs::write(b.path().join("distinct"), b"x").unwrap();

        import(repo_dir.path(), "gcc", a.path(), None, "MIT").unwrap();
        import(repo_dir.path(), "clang", b.path(), None, "Apache-2.0").unwrap();

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
        import(repo_dir.path(), "gcc", dir.path(), None, "MIT").unwrap();

        remove(repo_dir.path(), "gcc").unwrap();
        let _ = resolve(repo_dir.path(), "gcc").unwrap_err();
    }

    #[test]
    fn import_rejects_an_invalid_name() {
        let repo_dir = repo();
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        let result = import(repo_dir.path(), "not/valid", dir.path(), None, "MIT");
        assert!(matches!(result, Err(Error::InvalidName(_))));
    }
}
