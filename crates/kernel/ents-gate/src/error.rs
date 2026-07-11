//! The gate's infrastructure error type.
//!
//! An [`Error`] is never a verdict: it means the gate could not *reach* a
//! judgment (a store read failed, an object is missing or undecodable),
//! as opposed to [`crate::Refusal`], which is the judgment "no". Callers
//! at the mandatory call site (`gate.mandatory-hosted`) must treat an
//! `Error` exactly like a failing verdict — abort the write — because a
//! gate that cannot read its policy must fail closed; advisory call sites
//! should surface it as "could not evaluate", not as "refused".

use gix_hash::ObjectId;

/// Everything that can prevent the gate from reaching a verdict.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The ref store's read half failed. Retry or surface; the proposed
    /// update was neither admitted nor refused.
    #[error("ref store read failed: {0}")]
    Refs(#[from] gix_ref_store::Error),

    /// The object store failed while looking up `oid`.
    #[error("object lookup failed for {oid}: {source}")]
    Object {
        /// The object being looked up.
        oid: ObjectId,
        /// The underlying object-store error.
        #[source]
        source: gix_object::find::Error,
    },

    /// `oid` is not present in the object store. At the hosted call site
    /// this means the push's objects were not ingested before the gate
    /// ran; at pre-flight it usually means an unfetched object.
    #[error("object {oid} is missing from the object store")]
    Missing {
        /// The absent object.
        oid: ObjectId,
    },

    /// `oid` exists but could not be decoded as the object kind the gate
    /// needed (a commit, or a commit's timestamp field).
    #[error("object {oid} could not be decoded: {detail}")]
    Decode {
        /// The undecodable object.
        oid: ObjectId,
        /// What failed, human-readable.
        detail: String,
    },

    /// A policy entity's typed tree (a member, or `refs/meta/config`)
    /// could not be deserialized. The gate fails closed on this rather
    /// than treating unreadable policy as absent policy.
    #[error("policy entity at {oid} is unreadable: {source}")]
    Entity {
        /// The tree (or commit) whose entity failed to load.
        oid: ObjectId,
        /// The typed-tree deserialization error.
        #[source]
        source: facet_git_tree::Error,
    },
}

/// The `Result` alias every fallible `ents-gate` operation returns.
pub type Result<T> = std::result::Result<T, Error>;
