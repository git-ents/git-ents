//! `ents-receive`'s infrastructure error type.
//!
//! An [`Error`] means `receive` could not *reach* an outcome (a store read
//! failed, an object is missing or undecodable, an [`crate::EventSink`]
//! failed to durably enqueue) — as opposed to an ordinary refusal
//! ([`ents_gate::Verdict::Fail`], [`crate::TxResult::Refused`],
//! [`crate::TxResult::Rejected`], [`crate::TxResult::Redacted`]), which is a
//! reached judgment carried inside [`crate::Outcome`], not an `Err`.

use gix_hash::ObjectId;

/// Everything that can prevent `receive` from reaching an outcome.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The gate could not evaluate a proposed transition.
    #[error("gate evaluation failed: {0}")]
    Gate(#[from] ents_gate::Error),

    /// The ref store's read or write half failed.
    #[error("ref store operation failed: {0}")]
    Refs(#[from] gix_ref_store::Error),

    /// The query evaluator could not compute an entry or work set.
    #[error("query evaluation failed: {0}")]
    Eval(#[from] ents_query::EvalError),

    /// An `EventSink` failed to durably enqueue an obligation.
    ///
    /// `receive.never-blocks` still holds: this only ever wraps a genuine
    /// sink failure (durable-queue I/O, hosted), never effect evaluation
    /// itself, which this crate never performs.
    #[error("event sink failed to enqueue ({effect}, {oid}): {detail}")]
    Sink {
        /// The effect the obligation was for.
        effect: String,
        /// The commit the obligation names.
        oid: ObjectId,
        /// What failed, human-readable.
        detail: String,
    },

    /// An object could not be read or decoded while scanning
    /// `refs/meta/effects/*` or `refs/meta/redactions/*`.
    #[error("object {oid} could not be read: {detail}")]
    Decode {
        /// The undecodable object.
        oid: ObjectId,
        /// What failed, human-readable.
        detail: String,
    },

    /// A typed entity could not be serialized into a tree
    /// ([`crate::propose_entity`]'s first step).
    #[error("typed-tree operation failed: {0}")]
    Tree(#[from] facet_git_tree::Error),

    /// A built commit object could not be written to the object store
    /// ([`crate::propose_entity`]'s signed commit, or its bound-parent
    /// deletion transition).
    #[error("object write failed: {0}")]
    ObjectWrite(#[from] gix_object::write::Error),
}

/// The `Result` alias every fallible `ents-receive` operation returns.
pub type Result<T> = std::result::Result<T, Error>;
