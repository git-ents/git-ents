//! Backend-agnostic storage traits for git-ents.
//!
//! [`RefStore`], [`ObjectStore`], and [`EffectExecutor`] are the seams the
//! development plan (`docs/scale-out.adoc`, "Storage traits") draws between
//! application logic and where repository state actually lives. Application
//! code is written once, against these traits; a local backend
//! (`refstore-files`, `odb-files`) and a future cloud backend
//! (`refstore-postgres`, `odb-tigris`) both satisfy the same contract,
//! checked by a conformance suite (WS2) rather than assumed.
//!
//! # Why these three
//!
//! - [`RefStore`] is the unit of correctness: every write to repository
//!   state is a ref transaction, and multi-ref compare-and-swap is
//!   contractual, not optional.
//! - [`ObjectStore`] is deliberately narrower than a full git object
//!   database: there is no `write_loose`, because a remote object tier
//!   (Tigris) cannot offer one. Objects arrive as packs, staged in
//!   quarantine until the ref transaction that makes them reachable
//!   commits.
//! - [`EffectExecutor`] is the seam between the effect engine and wherever
//!   an effect actually runs (a local sandbox today, a Fly Sprite later).
//!
//! See `docs/scale-out.adoc` for the full rationale, the correctness rules
//! that bind every backend, and the workstream this crate implements (WS1).

pub mod cache_ns;
mod effect;
mod object_store;
mod ref_store;

pub use effect::{EffectDef, EffectExecutor, EffectHandle, EffectStatus, MaterializedInputs};
pub use object_store::{Object, ObjectStore, PackStream, QuarantineId};
pub use ref_store::{
    Expected, RefEdit, RefEvent, RefEventStream, RefIter, RefLogEntry, RefLogIter, RefName,
    RefStore, TxOutcome,
};

/// A failure in a [`RefStore`], [`ObjectStore`], or [`EffectExecutor`]
/// implementation. Shared across all three traits so application code
/// handles storage failures uniformly regardless of which seam raised them.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A [`RefStore`] operation failed for a reason other than a
    /// compare-and-swap mismatch, which is reported as
    /// [`TxOutcome::Rejected`] rather than an `Err`.
    #[error("ref store operation failed: {0}")]
    RefStore(String),
    /// An [`ObjectStore`] operation failed.
    #[error("object store operation failed: {0}")]
    ObjectStore(String),
    /// An [`EffectExecutor`] operation failed.
    #[error("effect executor operation failed: {0}")]
    Effect(String),
    /// An underlying I/O error.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
}

/// This crate's `Result` alias.
pub type Result<T> = std::result::Result<T, Error>;
