//! `git ents setup`: resolve or generate a signing key, record it as this
//! repository's `user.signingkey` with `gpg.format=ssh`, and set
//! `receive.denyCurrentBranch=updateInstead` (`roots.worktree-update`).
//!
//! `receive.denyCurrentBranch=updateInstead` is the integration-test
//! harness edge case `roots.worktree-update` names: it lets an external
//! push land on this repository's checked-out branch and still update the
//! working tree, which is not how a normal git remote behaves and is never
//! needed for `refs/meta/*` traffic (which never touches a worktree at
//! all).

use std::path::{Path, PathBuf};
use std::process::Command;

use rand_core::OsRng;
use ssh_key::{Algorithm, LineEnding, PrivateKey};

use crate::error::{Error, Result};
use crate::root::LocalRoot;
use crate::sign::Signer;

/// Run `git ents setup` against `root`: resolve `key`, generating a new
/// `~/.ssh/id_ed25519` if neither `key` nor `user.signingkey` resolves to
/// an existing file, then write `user.signingkey`, `gpg.format=ssh`, and
/// `receive.denyCurrentBranch=updateInstead` to the repository's own
/// (local) config.
///
/// # Errors
///
/// [`Error::BadSigningKey`] if a given or configured key cannot be loaded;
/// [`Error::Io`] if generating or writing a new key fails; propagates a
/// config-write failure.
pub fn run(root: &LocalRoot, key: Option<PathBuf>) -> Result<PathBuf> {
    let repo = gix::open(&root.path)?;
    let resolved = match crate::sign::resolve_key_path(&repo, key.as_deref()) {
        Ok(path) if path.exists() => path,
        Ok(path) => generate_key(&path)?,
        Err(Error::NoSigningKey) => {
            let default = default_key_path()?;
            generate_key(&default)?
        }
        Err(other) => return Err(other),
    };
    // Confirm the resolved key actually loads before recording it.
    Signer::load(&resolved)?;

    // `gix`'s own config-snapshot API (`config_snapshot_mut`) has no
    // file-persistence path at all: `SnapshotMut::commit` only updates the
    // in-memory resolved view this `Repository` handle holds, never
    // `.git/config` on disk (confirmed empirically — a value written that
    // way is invisible to a subsequent, separate `git config` read).
    // Writing durable local config is therefore delegated to `git config`
    // itself here, same as `pre-redo`'s own client setup did; it is not
    // part of the ref/object CAS discipline the rest of this crate is
    // strict about (`arch.loose-cas-discipline` governs refs, not plain
    // config values).
    let path_str = resolved.to_string_lossy().into_owned();
    for (key, value) in [
        ("user.signingkey", path_str.as_str()),
        ("gpg.format", "ssh"),
        ("receive.denyCurrentBranch", "updateInstead"),
    ] {
        set_local_config(&root.path, key, value)?;
    }

    Ok(resolved)
}

/// Set `key` to `value` in `repo_path`'s own local config via `git config`.
fn set_local_config(repo_path: &Path, key: &str, value: &str) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["config", "--local", key, value])
        .output()
        .map_err(|source| Error::Io {
            path: repo_path.to_owned(),
            source,
        })?;
    if !output.status.success() {
        return Err(Error::BadSigningKey {
            path: repo_path.to_owned(),
            detail: format!(
                "git config --local {key} {value} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ),
        });
    }
    Ok(())
}

/// Generate a fresh, unencrypted ed25519 key at `path` (creating parent
/// directories as needed) and return `path` unchanged.
fn generate_key(path: &Path) -> Result<PathBuf> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| Error::Io {
            path: parent.to_owned(),
            source,
        })?;
    }
    let key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).map_err(|source| {
        Error::BadSigningKey {
            path: path.to_owned(),
            detail: source.to_string(),
        }
    })?;
    key.write_openssh_file(path, LineEnding::LF)
        .map_err(|source| Error::BadSigningKey {
            path: path.to_owned(),
            detail: source.to_string(),
        })?;
    Ok(path.to_owned())
}

fn default_key_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or(Error::NoSigningKey)?;
    Ok(PathBuf::from(home).join(".ssh").join("id_ed25519"))
}
