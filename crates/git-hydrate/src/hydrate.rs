//! The read-path hydration step (`docs/scale-out.adoc`, WS0's read path):
//! copy a repository's registered packs into a local bare repository's
//! `objects/pack/`, idempotently.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use git_backend::Result;
use odb_tigris::registry::PackRegistry;
use odb_tigris::transport::BlobTransport;

/// Ensure `repo_path` is a bare repository on local (ephemeral) disk
/// carrying every pack `registry` has registered for `repo_id`, fetched
/// from `transport`.
///
/// Idempotent and cheap to call on every request: a pack already present
/// locally (named after its [`odb_tigris::registry::PackId`], so presence
/// is a plain file check) is never re-fetched. Nothing here is
/// correctness-bearing — ephemeral disk death just means the next call
/// starts from an empty `objects/pack/` and re-copies everything
/// (`docs/scale-out.adoc`: "Ephemeral disk death -> re-hydrate. Nothing
/// correctness-bearing on disk").
///
/// # Errors
///
/// Returns an error if the bare repository cannot be initialized, the
/// registry cannot be listed, or a pack/idx cannot be fetched or written.
pub fn ensure_hydrated<T, R>(
    repo_path: &Path,
    repo_id: &str,
    transport: &T,
    registry: &R,
) -> Result<()>
where
    T: BlobTransport,
    R: PackRegistry,
{
    if !is_bare_repo(repo_path) {
        init_bare_repo(repo_path)?;
    }
    let pack_dir = repo_path.join("objects").join("pack");
    std::fs::create_dir_all(&pack_dir)?;

    for record in registry.list(repo_id)? {
        let pack_path = pack_dir.join(format!("pack-{}.pack", record.id.as_str()));
        let idx_path = pack_dir.join(format!("pack-{}.idx", record.id.as_str()));
        if pack_path.is_file() && idx_path.is_file() {
            // Already hydrated from a previous request/instance — the
            // whole point of naming local files after the registry's own
            // pack id.
            continue;
        }
        let pack_bytes = transport.get(&record.pack_key)?;
        let idx_bytes = transport.get(&record.idx_key)?;
        atomic_write(&pack_path, &pack_bytes)?;
        atomic_write(&idx_path, &idx_bytes)?;
    }
    Ok(())
}

/// Whether `path` is the root of a bare git repository.
fn is_bare_repo(path: &Path) -> bool {
    path.join("HEAD").is_file() && path.join("objects").is_dir()
}

/// Create an empty bare repository at `repo_path`, creating parent
/// directories as needed.
fn init_bare_repo(repo_path: &Path) -> Result<()> {
    if let Some(parent) = repo_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let status = Command::new("git")
        .arg("init")
        .arg("--bare")
        .arg("-q")
        .arg(repo_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !status.success() {
        return Err(git_backend::Error::ObjectStore(
            "git init --bare failed while hydrating a repository".to_owned(),
        ));
    }
    Ok(())
}

/// Write `bytes` to `path` via a same-directory temp file and rename, so a
/// reader never observes a partially-written pack or idx.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp_path = tmp_path_for(path);
    std::fs::write(&tmp_path, bytes)?;
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

fn tmp_path_for(path: &Path) -> PathBuf {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    PathBuf::from(tmp)
}
