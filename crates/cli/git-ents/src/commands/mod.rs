//! One module per `git ents` subcommand family — [`crate::cli`]'s
//! definitions given a body. Each function here is a thin caller into a
//! library crate: [`crate::exe`] dispatches to these, never the other way
//! around, so the same logic is callable from a test without a terminal.
#![expect(
    clippy::let_underscore_must_use,
    reason = "rendering an advisory-gate verdict to a writer is best-effort; a broken pipe here \
              is not actionable"
)]

pub mod account;
pub mod agent;
pub mod bootstrap;
pub mod comment;
pub mod effect;
pub mod inbox;
pub mod issue;
pub mod login;
pub mod lsp;
pub mod members;
pub mod redact;
pub mod review;
pub mod serve;
pub mod setup;
pub mod toolchain;

use std::path::PathBuf;

use gix_hash::ObjectId;
use gix_object::{CommitRef, Find, Kind};

use crate::error::{Error, Result};
use crate::root::LocalRoot;
use crate::sign::Signer;

/// The tree of the commit at `oid` — every command that reads back a typed
/// entity needs this, and neither `ents_receive` nor `ents_effect` exports
/// their own copy publicly, so it is a small, shared utility here rather
/// than duplicated per command module.
///
/// # Errors
///
/// [`Error::NotFound`] if `oid` is missing or not a commit.
pub(crate) fn commit_tree(objects: &impl Find, oid: ObjectId) -> Result<ObjectId> {
    let mut buf = Vec::new();
    let data = objects
        .try_find(&oid, &mut buf)
        .map_err(|source| Error::InvalidArgument(source.to_string()))?
        .ok_or_else(|| Error::NotFound {
            what: oid.to_string(),
        })?;
    if data.kind != Kind::Commit {
        return Err(Error::NotFound {
            what: oid.to_string(),
        });
    }
    let commit = CommitRef::from_bytes(data.data, oid.kind())
        .map_err(|source| Error::InvalidArgument(source.to_string()))?;
    Ok(commit.tree())
}

/// Resolve `--key` (or the repository's `user.signingkey`, or the default
/// `~/.ssh/id_ed25519`) into a loaded [`Signer`] — the one place every
/// write-side command turns an optional key path into a usable identity.
///
/// # Errors
///
/// See [`crate::sign::resolve_key_path`] and [`Signer::load`].
pub fn signer(root: &LocalRoot, key: Option<PathBuf>) -> Result<Signer> {
    let repo = gix::open(&root.path)?;
    let path = crate::sign::resolve_key_path(&repo, key.as_deref())?;
    Signer::load(&path)
}

/// The commit author/committer signature every mutation this CLI produces
/// carries: the current wall-clock time, under a fixed name/email derived
/// from the signer's own key fingerprint (this crate never depends on
/// `user.name`/`user.email` being configured, mirroring
/// `gix-ref-store`'s own reflog-identity rationale).
#[must_use]
pub fn actor(signer: &Signer) -> gix::actor::Signature {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or_default();
    gix::actor::Signature {
        name: "git-ents".into(),
        email: format!("{}@git-ents.local", short_fingerprint(signer)).into(),
        time: gix::date::Time { seconds, offset: 0 },
    }
}

fn short_fingerprint(signer: &Signer) -> String {
    let key = signer.public_openssh();
    let hex = key
        .split_whitespace()
        .nth(1)
        .unwrap_or(&key)
        .chars()
        .take(12)
        .collect::<String>();
    if hex.is_empty() {
        "member".to_owned()
    } else {
        hex
    }
}
