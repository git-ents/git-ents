//! [`ObjectStore`] over the gitoxide object database on `objects/`
//! (including alternates) — the local default backend
//! (`docs/scale-out.adoc`, "ObjectStore").
//!
//! Quarantine mirrors receive-pack's own mechanism: [`OdbFiles::stage_pack`]
//! indexes an incoming pack into a scratch directory under `objects/` that
//! the main object database never scans (it only looks in
//! `objects/pack/`), so staged objects are invisible to `read`/`contains` —
//! and therefore to any reachability walk or GC built on them — until
//! [`OdbFiles::promote`] moves the pack into `objects/pack/` proper.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Mutex, MutexGuard, PoisonError};

use git_backend::{Error, Object, ObjectStore, PackStream, QuarantineId, Result};
use gix::objs::{Exists as _, FindExt as _};
use gix_hash::ObjectId;

/// A pack staged by [`OdbFiles::stage_pack`], not yet promoted: the
/// quarantine directory holding it, and the paths
/// `gix_pack::Bundle::write_to_directory` wrote within it.
struct Quarantine {
    dir: PathBuf,
    data_path: Option<PathBuf>,
    index_path: Option<PathBuf>,
    keep_path: Option<PathBuf>,
}

/// [`ObjectStore`] over the gitoxide object database on a repository's
/// `objects/` directory.
///
/// The [`gix::odb::Handle`] is held behind a [`Mutex`] rather than as a bare
/// field: its decode caches use interior mutability that isn't `Sync`, while
/// [`ObjectStore`] must be — application code holds a backend behind an
/// `Arc` and shares it across threads.
pub struct OdbFiles {
    objects_dir: PathBuf,
    odb: Mutex<gix::odb::Handle>,
    quarantines: Mutex<HashMap<QuarantineId, Quarantine>>,
}

impl OdbFiles {
    /// Open the object store for the repository at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        let repo = gix::open(path).map_err(|error| Error::ObjectStore(error.to_string()))?;
        let objects_dir = repo.common_dir().join("objects");
        let odb =
            gix::odb::at(&objects_dir).map_err(|error| Error::ObjectStore(error.to_string()))?;
        Ok(Self {
            objects_dir,
            odb: Mutex::new(odb),
            quarantines: Mutex::new(HashMap::new()),
        })
    }
}

impl ObjectStore for OdbFiles {
    fn read(&self, id: ObjectId) -> Result<Object> {
        let mut buf = Vec::new();
        let data = lock(&self.odb)
            .find(&id, &mut buf)
            .map_err(|error| Error::ObjectStore(error.to_string()))?;
        Ok(Object {
            kind: data.kind,
            data: data.data.to_vec(),
        })
    }

    fn contains(&self, id: ObjectId) -> Result<bool> {
        Ok(lock(&self.odb).exists(&id))
    }

    fn stage_pack(&self, pack: PackStream) -> Result<QuarantineId> {
        let id = QuarantineId::new(uuid::Uuid::new_v4().to_string());
        let dir = self.objects_dir.join("quarantine").join(id.as_str());
        std::fs::create_dir_all(&dir)?;

        let mut reader = std::io::BufReader::new(pack);
        let outcome = gix_pack::Bundle::write_to_directory(
            &mut reader,
            Some(&dir),
            &mut gix::progress::Discard,
            &AtomicBool::new(false),
            None::<gix::odb::Handle>,
            gix_pack::bundle::write::Options {
                object_hash: gix_hash::Kind::Sha1,
                ..Default::default()
            },
        )
        .map_err(|error| Error::ObjectStore(error.to_string()))?;

        lock(&self.quarantines).insert(
            id.clone(),
            Quarantine {
                dir,
                data_path: outcome.data_path,
                index_path: outcome.index_path,
                keep_path: outcome.keep_path,
            },
        );
        Ok(id)
    }

    fn promote(&self, q: QuarantineId) -> Result<()> {
        let quarantine = lock(&self.quarantines)
            .remove(&q)
            .ok_or_else(|| Error::ObjectStore(format!("unknown quarantine {q}")))?;
        let pack_dir = self.objects_dir.join("pack");
        std::fs::create_dir_all(&pack_dir)?;
        if let Some(data_path) = &quarantine.data_path {
            move_into(data_path, &pack_dir)?;
        }
        if let Some(index_path) = &quarantine.index_path {
            move_into(index_path, &pack_dir)?;
        }
        if let Some(keep_path) = &quarantine.keep_path {
            let _removed = std::fs::remove_file(keep_path);
        }
        let _removed = std::fs::remove_dir_all(&quarantine.dir);
        Ok(())
    }
}

/// Move the file at `path` into `dest_dir`, keeping its file name.
fn move_into(path: &Path, dest_dir: &Path) -> Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| Error::ObjectStore(format!("{path:?} has no file name")))?;
    std::fs::rename(path, dest_dir.join(name))?;
    Ok(())
}

/// Lock `mutex`, recovering the guard from a poisoned lock rather than
/// panicking — quarantine bookkeeping is not worth tearing the process
/// down over if an earlier panic poisoned it.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::let_underscore_must_use,
        reason = "unit test"
    )]

    use std::process::{Command, Stdio};

    use git_backend::{ObjectStore as _, PackStream};
    use git_store::test_support::{commit_all, head, repo};

    use super::OdbFiles;

    /// A real pack containing `commit` and everything it reaches, built by
    /// shelling out to `git rev-list`/`git pack-objects` against `dir` — the
    /// same mechanism a real push transmits, so `stage_pack` is exercised
    /// against pack bytes gitoxide's indexer actually has to parse.
    fn pack_for(dir: &std::path::Path, commit: &str) -> Vec<u8> {
        let mut rev_list = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["rev-list", "--objects", commit])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let pack_objects = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["pack-objects", "--stdout", "-q"])
            .stdin(rev_list.stdout.take().unwrap())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let output = pack_objects.wait_with_output().unwrap();
        assert!(rev_list.wait().unwrap().success());
        assert!(output.status.success());
        output.stdout
    }

    #[test]
    fn contains_is_false_for_an_object_the_store_never_saw() {
        let dir = repo();
        let store = OdbFiles::open(dir.path()).unwrap();
        let missing = gix_hash::ObjectId::null(gix_hash::Kind::Sha1);
        assert!(!store.contains(missing).unwrap());
    }

    #[test]
    fn staged_objects_are_invisible_until_promoted() {
        let source = repo();
        std::fs::write(source.path().join("file"), b"content").unwrap();
        commit_all(source.path(), "first");
        let commit_hex = head(source.path());
        let commit = gix_hash::ObjectId::from_hex(commit_hex.as_bytes()).unwrap();
        let pack_bytes = pack_for(source.path(), &commit_hex);

        // A separate, empty destination repository: the pack's objects
        // exist nowhere in it yet.
        let dest = repo();
        let store = OdbFiles::open(dest.path()).unwrap();
        assert!(!store.contains(commit).unwrap());

        let quarantine = store
            .stage_pack(PackStream::new(std::io::Cursor::new(pack_bytes)))
            .unwrap();
        // Staged, not promoted: still invisible.
        assert!(!store.contains(commit).unwrap());

        store.promote(quarantine).unwrap();
        assert!(store.contains(commit).unwrap());
        let object = store.read(commit).unwrap();
        assert_eq!(object.kind, gix_object::Kind::Commit);
    }
}
