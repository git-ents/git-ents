//! [`BlobTransport`]: the seam between `odb-tigris`'s object-store logic and
//! however bytes actually move to and from the bucket (`docs/scale-out.adoc`,
//! "ObjectStore" / WS5). Every method takes plain string keys so the rest of
//! the crate never has to know whether it's talking to S3 or a local
//! directory — [`fs::FsTransport`] is a no-network stand-in used by tests and
//! conformance, [`s3::S3Transport`] is the real one.
//!
//! # Q2: read-after-write visibility
//!
//! [`ObjectStore::promote`](git_backend::ObjectStore::promote) assumes that a
//! `put` (or `copy`) completed here is immediately visible to a subsequent
//! `get`/`get_range` against the same key — i.e. the bucket offers
//! read-after-write consistency for the keys this crate writes. Tigris (and
//! S3 today) document this, but it is a fact to verify against the real
//! service, not an assumption to bake in silently
//! (`docs/scale-out.adoc`, Q2).

pub mod fs;
pub mod s3;

use std::ops::Range;

use git_backend::{Error, Result};

/// One blob store, addressed by opaque string keys. Deliberately narrower
/// than a filesystem: no directories, no rename other than [`copy`], no
/// listing — the object-store layer above resolves everything it needs
/// (OID → pack, pack → key) through the [`crate::registry::PackRegistry`]
/// and cached pack indexes, never by asking the transport what keys exist
/// (`docs/scale-out.adoc`, "Reachability": never traverse the bucket
/// object-by-object).
pub trait BlobTransport: Send + Sync {
    /// Durably write `bytes` to `key`, replacing any prior content.
    fn put(&self, key: &str, bytes: Vec<u8>) -> Result<()>;

    /// Read the entirety of `key`.
    fn get(&self, key: &str) -> Result<Vec<u8>>;

    /// Read the byte range `range` of `key` — the operation the whole point
    /// of this crate exists to make cheap: an HTTP Range-GET of just the
    /// slice a caller needs, never the whole object. Implementations clamp
    /// `range.end` to the object's actual length rather than erroring, so
    /// callers can probe with a generous, possibly-too-large window (see
    /// `decode`'s growth loop) without special-casing the tail of a key.
    fn get_range(&self, key: &str, range: Range<u64>) -> Result<Vec<u8>>;

    /// Whether `key` exists.
    fn exists(&self, key: &str) -> Result<bool>;

    /// Remove `key`. Not an error if it does not exist.
    fn delete(&self, key: &str) -> Result<()>;

    /// Copy `from` to `to` without a round trip through the caller — used by
    /// [`crate::OdbTigris::promote`] to move a quarantined pack to its live
    /// key. All CAS (compare-and-swap) stays in Postgres via the pack
    /// registry; this is a plain durable copy, not a conditional write
    /// (`docs/scale-out.adoc`, WS5: "Tigris needs only durable PUT/GET").
    fn copy(&self, from: &str, to: &str) -> Result<()>;
}

// Every method takes `&self`, so a shared reference is itself a transport —
// lets one transport back both an `OdbTigris` and a maintenance pass
// (WS9's GC sweeps the same bucket the store reads).
impl<T: BlobTransport + ?Sized> BlobTransport for &T {
    fn put(&self, key: &str, bytes: Vec<u8>) -> Result<()> {
        (**self).put(key, bytes)
    }

    fn get(&self, key: &str) -> Result<Vec<u8>> {
        (**self).get(key)
    }

    fn get_range(&self, key: &str, range: Range<u64>) -> Result<Vec<u8>> {
        (**self).get_range(key, range)
    }

    fn exists(&self, key: &str) -> Result<bool> {
        (**self).exists(key)
    }

    fn delete(&self, key: &str) -> Result<()> {
        (**self).delete(key)
    }

    fn copy(&self, from: &str, to: &str) -> Result<()> {
        (**self).copy(from, to)
    }
}

/// Map any transport-level failure into [`git_backend::Error::ObjectStore`],
/// prefixed with `context` so failures are traceable to the operation that
/// caused them.
pub(crate) fn transport_err(context: &str, error: impl std::fmt::Display) -> Error {
    Error::ObjectStore(format!("{context}: {error}"))
}
