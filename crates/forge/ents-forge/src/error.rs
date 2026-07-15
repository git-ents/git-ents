//! `ents-forge`'s error type: everything that can prevent a comment
//! command from reaching a result.
//!
//! Mirrors `ents-effect`'s split: an [`Error`] here means the command could
//! not *reach* an outcome at all (a store or object read failed, an anchor
//! could not be captured or projected, `receive` itself could not reach a
//! judgment) — as opposed to a reached [`ents_receive::Outcome`], which
//! [`crate::comment::add`] returns as `Ok` for its caller to interpret (the
//! CLI's own `outcome_to_result`, for instance).

use std::path::PathBuf;

/// Everything that can prevent an `ents-forge` operation from reaching a
/// result.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The ref store's read or write half failed.
    #[error("ref store operation failed: {0}")]
    Refs(#[from] gix_ref_store::Error),

    /// A typed-tree entity (a [`crate::comment::Comment`] or
    /// [`ents_anchor::Anchor`]) could not be (de)serialized.
    #[error("typed-tree operation failed: {0}")]
    Tree(#[from] facet_git_tree::Error),

    /// Capturing or projecting a code anchor failed.
    #[error("anchor operation failed: {0}")]
    Anchor(#[from] ents_anchor::Error),

    /// Building a comment's refname failed (`ents_model::namespace`).
    #[error("model error: {0}")]
    Model(#[from] ents_model::Error),

    /// Opening the local git repository failed (`gix::open`, needed for
    /// `ents_anchor::capture`/`project`). Boxed: `gix::open::Error` is
    /// large enough on its own to trip `clippy::result_large_err` for
    /// every fallible function in this crate if stored inline, the same
    /// reasoning `ents-effect`'s own error type documents for its boxed
    /// `ents_receive::Error` variant.
    #[error(transparent)]
    Repo(Box<gix::open::Error>),

    /// `ents_receive::propose_entity` (or `receive` itself) could not
    /// reach an outcome. Boxed; see [`Error::Repo`]'s own doc.
    #[error("receive failed: {0}")]
    Receive(Box<ents_receive::Error>),

    /// The named entity (a comment) does not exist.
    #[error("not found: {what}")]
    NotFound {
        /// What was being looked up.
        what: String,
    },

    /// A malformed argument that fails a semantic check this crate makes
    /// (an invalid line range, an unparsable oid, ...).
    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    /// A local (non-git, non-gate) I/O failure: reading or writing a file
    /// outside the object database.
    #[error("io error at {path}: {source}")]
    Io {
        /// The path being read or written.
        path: PathBuf,
        /// The underlying I/O failure.
        #[source]
        source: std::io::Error,
    },
}

impl From<gix::open::Error> for Error {
    fn from(source: gix::open::Error) -> Self {
        Self::Repo(Box::new(source))
    }
}

impl From<ents_receive::Error> for Error {
    fn from(source: ents_receive::Error) -> Self {
        Self::Receive(Box::new(source))
    }
}

/// The `Result` alias every fallible `ents-forge` operation returns.
pub type Result<T> = std::result::Result<T, Error>;
