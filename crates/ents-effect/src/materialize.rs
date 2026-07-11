//! Checking out a git tree onto disk through gitoxide's `Find` seam alone
//! (`arch.no-object-store-trait`) — no dependency on a real on-disk `.git`
//! directory or a `git archive` subprocess, so this works identically
//! against the in-memory fixture store in tests and a real odb in
//! production.
//!
//! This is the one code path both the run loop's pushed-tree checkout and
//! [`crate::toolchain::materialize`]'s `Embedded` case share
//! (`effect.local-run`: "identical code path").

use std::path::Path;

use gix_hash::ObjectId;
use gix_object::bstr::ByteSlice as _;
use gix_object::tree::EntryKind;
use gix_object::{Find, Kind, TreeRef};

use crate::error::{Error, Result};

/// Recursively write `tree`'s entries under `dest`, which must already
/// exist. Blob entries are written verbatim, with the executable bit set
/// per the entry's mode; tree entries recurse into a created subdirectory.
///
/// # Errors
///
/// [`Error::Submodule`] for a gitlink entry (this design embeds no
/// submodule content, `effect.toolchains`'s neighboring retention rule);
/// [`Error::NotUtf8`] for a non-UTF-8 filename; [`Error::Missing`] or
/// [`Error::Decode`] for an unreadable object; [`Error::Io`] for a host
/// filesystem failure. A symlink entry is written as a real symlink
/// (`std::os::unix::fs::symlink`) pointing at its recorded target text.
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

    for (name, kind, oid) in entries {
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
}
