//! `ents-sync`'s infrastructure error type.
//!
//! An [`Error`] means sync could not *reach* a result — an object could
//! not be read or written, a ref store failed, a commit did not decode.
//! It is never a merge conflict or a negative verdict: a
//! [`crate::Conflict`] and a [`ents_gate::Verdict::Fail`] are both reached
//! results the caller acts on, not failures to compute one.

use gix_hash::ObjectId;

/// Everything that can prevent sync from reaching a result.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A ref store (local or remote) failed. The transfer or verdict was
    /// neither completed nor refused; retry or surface.
    #[error("ref store failed: {0}")]
    Refs(#[from] gix_ref_store::Error),

    /// The gate could not evaluate during push pre-flight
    /// (`sync.pre-flight`). Distinct from a failing verdict, which is a
    /// reached prediction; this is the gate itself failing to compute one.
    #[error("gate evaluation failed: {0}")]
    Gate(#[from] ents_gate::Error),

    /// An object lookup failed while reading `oid`.
    #[error("object lookup failed for {oid}: {source}")]
    Object {
        /// The object being looked up.
        oid: ObjectId,
        /// The underlying object-store error.
        #[source]
        source: gix_object::find::Error,
    },

    /// Writing an object into the destination store failed while
    /// transferring the forge (`sync.forge-transfer`).
    #[error("object write failed: {0}")]
    Write(#[from] gix_object::write::Error),

    /// `oid` is absent from the store it was expected in. During transfer
    /// this means the source is missing an object its own ref reaches;
    /// during a merge it means a proposed head is not present locally.
    #[error("object {oid} is missing")]
    Missing {
        /// The absent object.
        oid: ObjectId,
    },

    /// `oid` exists but could not be decoded as the object kind sync
    /// needed (a commit while walking history, or a tree while merging).
    #[error("object {oid} could not be decoded: {detail}")]
    Decode {
        /// The undecodable object.
        oid: ObjectId,
        /// What failed, human-readable.
        detail: String,
    },

    /// A refname could not be constructed while routing to the inbox
    /// (`sync.inbox-routing`) — for example, a canonical suffix that is not
    /// a valid ref component.
    #[error("invalid refname while routing to inbox: {0}")]
    RefName(#[from] ents_model::Error),
}

/// The `Result` alias every fallible `ents-sync` operation returns.
pub type Result<T> = std::result::Result<T, Error>;
