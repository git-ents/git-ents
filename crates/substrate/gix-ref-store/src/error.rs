//! The error type every `gix-ref-store` operation returns.

use std::path::PathBuf;

/// Everything that can go wrong reading or writing through a [`crate::RefStore`].
///
/// Every variant is a backend I/O or protocol failure; a *rejected*
/// compare-and-swap is not an error at all, since a stale precondition is
/// an expected outcome, not a fault. See [`crate::TxOutcome::Rejected`].
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Opening the on-disk repository the store reads and writes through
    /// failed. The caller should check that `path` names a git repository
    /// (or its `.git` directory) and that the process has permission to
    /// read it.
    #[error("failed to open the repository at {path}: {source}")]
    Open {
        /// The path that was passed to [`crate::LooseRefStore::open`].
        path: PathBuf,
        /// The underlying gitoxide error.
        #[source]
        source: Box<gix::open::Error>,
    },

    /// A refname string failed gitoxide's own validation (for example, it
    /// contained a `..` component or a disallowed character). The caller
    /// should reject the name before offering it to a [`crate::RefStore`].
    #[error("invalid reference name: {0}")]
    InvalidName(#[from] gix::validate::reference::name::Error),

    /// A read (lookup, peel, or iteration) against the backend failed for
    /// a reason other than the ref simply not existing. This wraps
    /// whatever gitoxide's own read path reported; the caller should treat
    /// it as an I/O-class failure, not a CAS rejection.
    #[error("ref-store read failed: {0}")]
    Read(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),

    /// A [`crate::RefStore::transaction`] call failed outright — a lock
    /// could not be acquired, the on-disk state could not be parsed, or
    /// similar — as distinct from a clean CAS rejection, which is
    /// reported as `Ok(TxOutcome::Rejected { .. })` rather than this
    /// variant.
    #[error("ref transaction failed: {0}")]
    Transaction(#[from] gix::reference::edit::Error),

    /// The store's own serialization lock (see `loose` module docs for why
    /// it exists) could not be acquired within its timeout — most likely
    /// another `transaction()` call is legitimately in flight and slow, or
    /// a prior process crashed while holding it and left the lock file
    /// behind. The caller should retry, and an operator investigating a
    /// permanently-stuck store should look for a stale lock file in the
    /// repository's git directory.
    #[error("could not acquire the ref-store transaction lock: {0}")]
    StoreLock(#[source] gix_lock::acquire::Error),
}

/// The `Result` alias every `gix-ref-store` operation returns.
pub type Result<T> = std::result::Result<T, Error>;
