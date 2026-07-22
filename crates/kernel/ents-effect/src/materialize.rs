//! Checking out a git tree onto disk through gitoxide's `Find` seam alone
//! (`arch.no-object-store-trait`) — no dependency on a real on-disk `.git`
//! directory or a `git archive` subprocess, so this works identically
//! against the in-memory fixture store in tests and a real odb in
//! production.
//!
//! This is the one code path both the run loop's pushed-tree checkout and
//! `ents-kiln`'s toolchain `materialize`'s `Embedded` case share
//! (`effect.local-run`: "identical code path") — this module is `pub` so
//! `ents-kiln` can call [`checkout`] directly across the crate boundary.
//!
//! Checkout runs on the *host*, before any sandbox exists, so it defends
//! itself against a crafted tree (fsck-invalid but storable, and a trigger
//! can match any pushed commit): entry names that could escape or collide
//! inside `dest` — `.`, `..`, path separators, duplicates — are refused
//! before anything is written ([`Error::UnsafeEntry`]), and within each
//! tree symlink entries are written only after every other entry, so no
//! write in the same checkout can be routed *through* a symlink the tree
//! itself planted.

use std::collections::HashSet;
use std::path::Path;

use gix_hash::ObjectId;
use gix_object::bstr::ByteSlice as _;
use gix_object::tree::{Entry, EntryKind, EntryMode};
use gix_object::{Find, Kind, Tree, TreeRef, Write};

use crate::error::{Error, Result};

/// Whether `name` is a single, plain path component that cannot escape or
/// alias its parent directory.
fn safe_entry_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
}

/// Recursively write `tree`'s entries under `dest`, which must already
/// exist. Blob entries are written verbatim, with the executable bit set
/// per the entry's mode; tree entries recurse into a created subdirectory;
/// within each tree, symlink entries are written after every other entry
/// (see the module doc for why).
///
/// # Errors
///
/// [`Error::UnsafeEntry`] for an entry name that could escape `dest` or
/// duplicate an earlier entry; [`Error::Submodule`] for a gitlink entry
/// (this design embeds no submodule content, `effect.toolchains`'s
/// neighboring retention rule); [`Error::NotUtf8`] for a non-UTF-8
/// filename; [`Error::Missing`] or [`Error::Decode`] for an unreadable
/// object; [`Error::Io`] for a host filesystem failure. A symlink entry is
/// written as a real symlink (`std::os::unix::fs::symlink`) pointing at
/// its recorded target text.
// @relation(effect.execution, scope=function)
pub fn checkout(objects: &impl Find, tree: ObjectId, dest: &Path) -> Result<()> {
    let mut buf = Vec::new();
    let data = objects
        .try_find(&tree, &mut buf)
        .map_err(|source| Error::Decode {
            oid: tree,
            detail: source.to_string(),
        })?
        .ok_or(Error::Missing { oid: tree })?;
    if data.kind != Kind::Tree {
        return Err(Error::Decode {
            oid: tree,
            detail: "expected a tree".to_owned(),
        });
    }
    let entries: Vec<(String, EntryKind, ObjectId)> = TreeRef::from_bytes(data.data, tree.kind())
        .map_err(|e| Error::Decode {
            oid: tree,
            detail: e.to_string(),
        })?
        .entries
        .iter()
        .map(|entry| {
            let name = entry
                .filename
                .to_str()
                .map_err(|_not_utf8| Error::NotUtf8(dest.join(entry.filename.to_string())))?
                .to_owned();
            Ok((name, entry.mode.kind(), entry.oid.to_owned()))
        })
        .collect::<Result<Vec<_>>>()?;

    let mut seen: HashSet<&str> = HashSet::with_capacity(entries.len());
    for (name, _, _) in &entries {
        if !safe_entry_name(name) {
            return Err(Error::UnsafeEntry {
                name: name.clone(),
                detail: "not a plain single path component".to_owned(),
            });
        }
        if !seen.insert(name.as_str()) {
            return Err(Error::UnsafeEntry {
                name: name.clone(),
                detail: "duplicate entry in one tree".to_owned(),
            });
        }
    }

    // Symlinks last: nothing else written by this checkout can be routed
    // through a link the same tree planted.
    let (links, others): (Vec<_>, Vec<_>) = entries
        .into_iter()
        .partition(|(_, kind, _)| *kind == EntryKind::Link);

    for (name, kind, oid) in others.into_iter().chain(links) {
        let path = dest.join(&name);
        match kind {
            EntryKind::Tree => {
                make_dir(&path)?;
                checkout(objects, oid, &path)?;
            }
            EntryKind::Commit => {
                return Err(Error::Submodule { path: name });
            }
            EntryKind::Link => {
                let mut buf = Vec::new();
                let data = objects
                    .try_find(&oid, &mut buf)
                    .map_err(|source| Error::Decode {
                        oid,
                        detail: source.to_string(),
                    })?
                    .ok_or(Error::Missing { oid })?;
                let target = std::str::from_utf8(data.data)
                    .map_err(|_not_utf8| Error::NotUtf8(path.clone()))?;
                symlink(target, &path)?;
            }
            EntryKind::Blob | EntryKind::BlobExecutable => {
                let mut buf = Vec::new();
                let data = objects
                    .try_find(&oid, &mut buf)
                    .map_err(|source| Error::Decode {
                        oid,
                        detail: source.to_string(),
                    })?
                    .ok_or(Error::Missing { oid })?;
                std::fs::write(&path, data.data).map_err(|source| Error::Io {
                    path: path.clone(),
                    source,
                })?;
                if kind == EntryKind::BlobExecutable {
                    set_executable(&path)?;
                }
            }
        }
    }
    Ok(())
}

/// The reverse of [`checkout`]: recursively read `src`'s current on-disk
/// state and write it as a git tree, preserving the executable bit and
/// symlink targets `checkout` itself would restore. A directory walk can
/// never discover a gitlink (there is no way for a plain host directory to
/// carry one), so unlike `checkout` this has no submodule case to refuse.
///
/// This is `docs/agent-sessions-plan.adoc`'s Phase 2 finalize's other
/// half from `checkout`: after [`crate::Executor::run`] completes, a
/// composition root reads the checked-out workdir's now-current state back
/// through this function to build the sandbox's output tree — the tree the
/// result branch's commit carries. Every effect backend built so far in
/// this crate (`UnsandboxedExecutor` today; a future `SpriteExecutor` that
/// syncs its sandbox's filesystem back onto `workdir` before returning)
/// leaves its command's file-level effects on `workdir` itself, so this
/// reads the same host directory [`SandboxInputs::workdir`] named, not a
/// second, backend-specific location.
///
/// # Errors
///
/// [`Error::NotUtf8`] for a non-UTF-8 entry name or symlink target;
/// [`Error::Io`] for a host filesystem failure reading `src` or one of its
/// entries; [`Error::ObjectWrite`] if writing a blob or tree object fails.
///
/// # Examples
///
/// ```
/// use ents_effect::materialize::write_tree;
/// use ents_testutil::ObjectStore;
///
/// let dir = tempfile::tempdir().expect("tempdir");
/// std::fs::write(dir.path().join("README"), b"hello\n").expect("write");
/// std::fs::create_dir(dir.path().join("sub")).expect("mkdir");
/// std::fs::write(dir.path().join("sub/leaf.txt"), b"leaf\n").expect("write");
///
/// let objects = ObjectStore::default();
/// let tree = write_tree(&objects, dir.path()).expect("writes");
///
/// let checked_out = tempfile::tempdir().expect("tempdir");
/// ents_effect::materialize::checkout(&objects, tree, checked_out.path()).expect("checkout");
/// assert_eq!(
///     std::fs::read_to_string(checked_out.path().join("README")).expect("read"),
///     "hello\n"
/// );
/// assert_eq!(
///     std::fs::read_to_string(checked_out.path().join("sub").join("leaf.txt")).expect("read"),
///     "leaf\n"
/// );
/// ```
// @relation(effect.execution, scope=function)
pub fn write_tree(objects: &(impl Find + Write), src: &Path) -> Result<ObjectId> {
    let read_dir = std::fs::read_dir(src).map_err(|source| Error::Io {
        path: src.to_owned(),
        source,
    })?;

    let mut entries = Vec::new();
    for dir_entry in read_dir {
        let dir_entry = dir_entry.map_err(|source| Error::Io {
            path: src.to_owned(),
            source,
        })?;
        let path = dir_entry.path();
        let name = dir_entry
            .file_name()
            .into_string()
            .map_err(|_not_utf8| Error::NotUtf8(path.clone()))?;
        let file_type = dir_entry.file_type().map_err(|source| Error::Io {
            path: path.clone(),
            source,
        })?;

        let (mode, oid) = if file_type.is_dir() {
            (
                EntryMode::from(EntryKind::Tree),
                write_tree(objects, &path)?,
            )
        } else if file_type.is_symlink() {
            let target = std::fs::read_link(&path).map_err(|source| Error::Io {
                path: path.clone(),
                source,
            })?;
            let target = target
                .to_str()
                .ok_or_else(|| Error::NotUtf8(path.clone()))?;
            let oid = objects.write_buf(Kind::Blob, target.as_bytes())?;
            (EntryMode::from(EntryKind::Link), oid)
        } else {
            let bytes = std::fs::read(&path).map_err(|source| Error::Io {
                path: path.clone(),
                source,
            })?;
            let kind = if is_executable(&path)? {
                EntryKind::BlobExecutable
            } else {
                EntryKind::Blob
            };
            (
                EntryMode::from(kind),
                objects.write_buf(Kind::Blob, &bytes)?,
            )
        };
        entries.push(Entry {
            mode,
            filename: name.into(),
            oid,
        });
    }
    // `gix_object::Tree::write_to` debug-asserts its entries are sorted by
    // its own `Ord` (git's tree-entry order, a directory compared as if it
    // carried a trailing `/`) — a plain directory walk has no such
    // ordering, so this sorts before handing the tree to `objects.write`.
    entries.sort();
    Ok(objects.write(&Tree { entries })?)
}

#[cfg(unix)]
fn is_executable(path: &Path) -> Result<bool> {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = std::fs::metadata(path)
        .map_err(|source| Error::Io {
            path: path.to_owned(),
            source,
        })?
        .permissions()
        .mode();
    Ok(mode & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> Result<bool> {
    Ok(false)
}

fn make_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path).map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let mut perms = std::fs::metadata(path)
        .map_err(|source| Error::Io {
            path: path.to_owned(),
            source,
        })?
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn symlink(target: &str, path: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, path).map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })
}

#[cfg(not(unix))]
fn symlink(target: &str, path: &Path) -> Result<()> {
    std::fs::write(path, target).map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "unit test")]

    use ents_testutil::ObjectStore;
    use gix_object::tree::{Entry, EntryMode};
    use gix_object::{Kind, Tree, Write as _};

    use super::*;

    #[test]
    // @relation(effect.toolchains, effect.execution, scope=function, role=Verifies)
    fn checkout_writes_blobs_and_sets_the_executable_bit() {
        let objects = ObjectStore::default();
        let script = objects
            .write_buf(Kind::Blob, b"#!/bin/sh\necho hi\n")
            .expect("write");
        let readme = objects.write_buf(Kind::Blob, b"hello\n").expect("write");
        let tree = Tree {
            entries: vec![
                Entry {
                    mode: EntryMode::from(EntryKind::Blob),
                    filename: "README".into(),
                    oid: readme,
                },
                Entry {
                    mode: EntryMode::from(EntryKind::BlobExecutable),
                    filename: "run.sh".into(),
                    oid: script,
                },
            ],
        };
        let tree_oid = objects.write(&tree).expect("write tree");

        let dir = tempfile::tempdir().expect("tempdir");
        checkout(&objects, tree_oid, dir.path()).expect("checkout");

        let script_path = dir.path().join("run.sh");
        assert_eq!(
            std::fs::read_to_string(&script_path).expect("read"),
            "#!/bin/sh\necho hi\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&script_path)
                .expect("stat")
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111, "run.sh must be executable");
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join("README")).expect("read"),
            "hello\n"
        );
    }

    #[test]
    // @relation(effect.toolchains, scope=function, role=Verifies)
    fn checkout_recurses_into_subdirectories() {
        let objects = ObjectStore::default();
        let leaf = objects.write_buf(Kind::Blob, b"leaf\n").expect("write");
        let inner = Tree {
            entries: vec![Entry {
                mode: EntryMode::from(EntryKind::Blob),
                filename: "leaf.txt".into(),
                oid: leaf,
            }],
        };
        let inner_oid = objects.write(&inner).expect("write inner tree");
        let outer = Tree {
            entries: vec![Entry {
                mode: EntryMode::from(EntryKind::Tree),
                filename: "sub".into(),
                oid: inner_oid,
            }],
        };
        let outer_oid = objects.write(&outer).expect("write outer tree");

        let dir = tempfile::tempdir().expect("tempdir");
        checkout(&objects, outer_oid, dir.path()).expect("checkout");

        assert_eq!(
            std::fs::read_to_string(dir.path().join("sub").join("leaf.txt")).expect("read"),
            "leaf\n"
        );
    }

    #[test]
    // @relation(effect.toolchains, scope=function, role=Verifies)
    fn checkout_refuses_a_submodule_entry() {
        let objects = ObjectStore::default();
        let tree = Tree {
            entries: vec![Entry {
                mode: EntryMode::from(EntryKind::Commit),
                filename: "vendor".into(),
                oid: ObjectId::null(gix_hash::Kind::Sha1),
            }],
        };
        let tree_oid = objects.write(&tree).expect("write tree");

        let dir = tempfile::tempdir().expect("tempdir");
        let err = checkout(&objects, tree_oid, dir.path()).expect_err("must refuse a gitlink");
        assert!(matches!(err, Error::Submodule { .. }));
    }

    #[test]
    // @relation(effect.execution, scope=function, role=Verifies)
    fn checkout_writes_a_symlink_entry_with_its_recorded_target() {
        let objects = ObjectStore::default();
        let target = objects.write_buf(Kind::Blob, b"README").expect("write");
        let tree = Tree {
            entries: vec![Entry {
                mode: EntryMode::from(EntryKind::Link),
                filename: "link".into(),
                oid: target,
            }],
        };
        let tree_oid = objects.write(&tree).expect("write tree");

        let dir = tempfile::tempdir().expect("tempdir");
        checkout(&objects, tree_oid, dir.path()).expect("checkout");

        #[cfg(unix)]
        assert_eq!(
            std::fs::read_link(dir.path().join("link")).expect("is a symlink"),
            std::path::PathBuf::from("README")
        );
    }

    /// One raw (git wire format) tree entry — the fsck-invalid trees these
    /// tests need cannot be built through gitoxide's own `Tree` writer,
    /// which validates sorting; the attack ships bytes, so the tests do
    /// too.
    fn raw_entry(mode: &str, name: &[u8], oid: &ObjectId) -> Vec<u8> {
        let mut entry = Vec::new();
        entry.extend_from_slice(mode.as_bytes());
        entry.push(b' ');
        entry.extend_from_slice(name);
        entry.push(0);
        entry.extend_from_slice(oid.as_bytes());
        entry
    }

    #[rstest::rstest]
    #[case::parent_dir(b"..".as_slice())]
    #[case::current_dir(b".".as_slice())]
    #[case::path_separator(b"a/b".as_slice())]
    #[case::backslash(b"a\\b".as_slice())]
    // @relation(effect.execution, scope=function, role=Verifies)
    fn checkout_refuses_an_entry_name_that_could_escape_the_destination(#[case] name: &[u8]) {
        let objects = ObjectStore::default();
        let blob = objects.write_buf(Kind::Blob, b"owned\n").expect("write");
        let tree_oid = objects
            .write_buf(Kind::Tree, &raw_entry("100644", name, &blob))
            .expect("a crafted tree is storable even though fsck-invalid");

        let dir = tempfile::tempdir().expect("tempdir");
        let err = checkout(&objects, tree_oid, dir.path())
            .expect_err("host-side checkout must refuse a traversal-shaped name");
        assert!(matches!(err, Error::UnsafeEntry { .. }), "got {err:?}");
        // Nothing may have been written before the refusal.
        assert_eq!(
            std::fs::read_dir(dir.path()).expect("readable").count(),
            0,
            "the refusal must come before any write"
        );
    }

    #[test]
    // @relation(effect.execution, scope=function, role=Verifies)
    fn checkout_refuses_duplicate_entries_in_one_tree() {
        // The concrete attack: a symlink named `sub` pointing outside the
        // destination, then a tree entry also named `sub` — without the
        // duplicate check, the recursion would write through the link.
        let objects = ObjectStore::default();
        let link_target = objects.write_buf(Kind::Blob, b"/tmp").expect("write");
        let payload = objects.write_buf(Kind::Blob, b"escaped\n").expect("write");
        let inner = Tree {
            entries: vec![Entry {
                mode: EntryMode::from(EntryKind::Blob),
                filename: "payload".into(),
                oid: payload,
            }],
        };
        let inner_oid = objects.write(&inner).expect("write inner tree");

        let mut raw = raw_entry("120000", b"sub", &link_target);
        raw.extend_from_slice(&raw_entry("40000", b"sub", &inner_oid));
        let tree_oid = objects
            .write_buf(Kind::Tree, &raw)
            .expect("a crafted tree is storable even though fsck-invalid");

        let dir = tempfile::tempdir().expect("tempdir");
        let err =
            checkout(&objects, tree_oid, dir.path()).expect_err("duplicate names must be refused");
        assert!(matches!(err, Error::UnsafeEntry { .. }), "got {err:?}");
    }

    #[test]
    // @relation(effect.execution, scope=function, role=Verifies)
    fn checkout_writes_symlinks_after_every_other_entry() {
        // A symlink sorted before a blob in the raw bytes must still be
        // created after it — the ordering is behavioral, not cosmetic: it
        // is what guarantees no later write in the same checkout can be
        // routed through a link the tree planted.
        let objects = ObjectStore::default();
        let link_target = objects.write_buf(Kind::Blob, b"z-file").expect("write");
        let blob = objects.write_buf(Kind::Blob, b"content\n").expect("write");
        let tree = Tree {
            entries: vec![
                Entry {
                    mode: EntryMode::from(EntryKind::Link),
                    filename: "a-link".into(),
                    oid: link_target,
                },
                Entry {
                    mode: EntryMode::from(EntryKind::Blob),
                    filename: "z-file".into(),
                    oid: blob,
                },
            ],
        };
        let tree_oid = objects.write(&tree).expect("write tree");

        let dir = tempfile::tempdir().expect("tempdir");
        checkout(&objects, tree_oid, dir.path()).expect("checkout");

        // Had the link been written first and the blob written through it,
        // reading via the link and via the file would still agree — so
        // assert on the filesystem's own record instead: the link must be
        // a symlink, and the file a regular file, each with its own bytes.
        let link_meta = std::fs::symlink_metadata(dir.path().join("a-link")).expect("stat");
        assert!(link_meta.file_type().is_symlink());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("z-file")).expect("read"),
            "content\n"
        );
    }

    // -----------------------------------------------------------------
    // write_tree: checkout's reverse.
    // -----------------------------------------------------------------

    #[test]
    // @relation(effect.execution, scope=function, role=Verifies)
    fn write_tree_round_trips_plain_files_and_subdirectories_through_checkout() {
        let objects = ObjectStore::default();
        let src = tempfile::tempdir().expect("tempdir");
        std::fs::write(src.path().join("README"), b"hello\n").expect("write");
        std::fs::create_dir(src.path().join("sub")).expect("mkdir");
        std::fs::write(src.path().join("sub/leaf.txt"), b"leaf\n").expect("write");

        let tree = write_tree(&objects, src.path()).expect("writes");

        let dest = tempfile::tempdir().expect("tempdir");
        checkout(&objects, tree, dest.path()).expect("checkout");
        assert_eq!(
            std::fs::read_to_string(dest.path().join("README")).expect("read"),
            "hello\n"
        );
        assert_eq!(
            std::fs::read_to_string(dest.path().join("sub").join("leaf.txt")).expect("read"),
            "leaf\n"
        );
    }

    #[test]
    // @relation(effect.execution, scope=function, role=Verifies)
    fn write_tree_preserves_the_executable_bit() {
        let objects = ObjectStore::default();
        let src = tempfile::tempdir().expect("tempdir");
        let script = src.path().join("run.sh");
        std::fs::write(&script, b"#!/bin/sh\necho hi\n").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mut perms = std::fs::metadata(&script).expect("stat").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).expect("chmod");
        }

        let tree = write_tree(&objects, src.path()).expect("writes");
        let dest = tempfile::tempdir().expect("tempdir");
        checkout(&objects, tree, dest.path()).expect("checkout");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(dest.path().join("run.sh"))
                .expect("stat")
                .permissions()
                .mode();
            assert_eq!(
                mode & 0o111,
                0o111,
                "the executable bit must survive the round trip"
            );
        }
    }

    #[test]
    // @relation(effect.execution, scope=function, role=Verifies)
    fn write_tree_entries_are_sorted_for_serialization() {
        // A directory walk yields entries in whatever order the host
        // filesystem happens to return them, never guaranteed to already be
        // git's own tree-entry order; `write_tree` must sort them itself so
        // `Tree::write_to`'s debug assertion never trips.
        let objects = ObjectStore::default();
        let src = tempfile::tempdir().expect("tempdir");
        for name in ["z-file", "a-file", "m-dir"] {
            if name == "m-dir" {
                std::fs::create_dir(src.path().join(name)).expect("mkdir");
            } else {
                std::fs::write(src.path().join(name), b"x").expect("write");
            }
        }
        write_tree(&objects, src.path()).expect("writes without tripping the sort assertion");
    }
}
