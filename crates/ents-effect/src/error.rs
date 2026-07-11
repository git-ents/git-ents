//! `ents-effect`'s error type: everything that can prevent a run from
//! reaching a recorded outcome.
//!
//! Mirrors `ents-receive`'s split: an [`Error`] means the run never reached
//! a judgment at all (a store or object read failed, a sandbox never
//! started, `curl`/`tar`/`docker`/`sprite` was not on `PATH`) — as opposed
//! to a completed run reporting `pass` or `fail`
//! (`effect.result-taxonomy`), which is a reached judgment, not an `Err`.
//! Per `effect.result-taxonomy`, an [`Error`] here is exactly the "queue
//! concern with bounded retry" case: this crate never turns one into a
//! `Status::Error` result itself — only a caller that has exhausted its own
//! retry bound does that, by calling [`crate::write_result`] with
//! `Status::Error` explicitly.

use std::path::PathBuf;

use gix_hash::ObjectId;

/// Everything that can prevent an `ents-effect` operation from reaching a
/// result.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The ref store's read or write half failed.
    #[error("ref store operation failed: {0}")]
    Refs(#[from] gix_ref_store::Error),

    /// `receive` (the write-back path, `effect.results-writeback`) could
    /// not reach an outcome.
    ///
    /// Boxed rather than `#[from]`-derived inline (see [`From`] below):
    /// `ents_receive::Error` embeds `ents_gate::Error`, which is large
    /// enough on its own to trip `clippy::result_large_err` for every
    /// fallible function in this crate if stored inline.
    #[error("receive failed: {0}")]
    Receive(Box<ents_receive::Error>),

    /// The query evaluator could not compute an effect's work set.
    #[error("query evaluation failed: {0}")]
    Eval(#[from] ents_query::EvalError),

    /// An effect's `trigger` failed to parse as a `CommitQuery`
    /// (`effect.validation`).
    #[error("trigger does not parse as a CommitQuery: {0}")]
    Trigger(#[from] ents_query::ParseError),

    /// A typed-tree entity (a [`ents_model::Toolchain`] or
    /// [`ents_model::Status`]) could not be (de)serialized.
    #[error("typed-tree operation failed: {0}")]
    Facet(#[from] facet_git_tree::Error),

    /// An object could not be read or decoded.
    #[error("object {oid} could not be read: {detail}")]
    Decode {
        /// The undecodable object.
        oid: ObjectId,
        /// What failed, human-readable.
        detail: String,
    },

    /// An object referenced by a tree or commit is missing from the object
    /// store.
    #[error("object {oid} is missing")]
    Missing {
        /// The missing object.
        oid: ObjectId,
    },

    /// `refs/meta/toolchains/<name>` does not exist.
    #[error("no toolchain named {0:?}")]
    UnknownToolchain(String),

    /// A toolchain's `recipe` field did not parse as a [`crate::Recipe`]
    /// (`effect.toolchains`: "a manifest's declared components MUST be
    /// resolved during effect execution").
    #[error("toolchain {name:?} has an unreadable recipe: {detail}")]
    InvalidRecipe {
        /// The toolchain's name.
        name: String,
        /// What failed, human-readable.
        detail: String,
    },

    /// An effect's `toolchains` list named something that is not a valid
    /// ref-path segment (`effect.validation`).
    #[error("{0:?} is not a valid toolchain name")]
    InvalidToolchainName(String),

    /// A materialized tree entry was a git submodule (a commit entry).
    /// Gitlinks retain nothing in this design (no embedded submodule
    /// content), so materializing one is refused rather than silently
    /// skipped.
    #[error("cannot materialize {path:?}: it is a git submodule (gitlink)")]
    Submodule {
        /// The offending path, relative to the materialization root.
        path: String,
    },

    /// A tree entry's filename, or a downloaded component's extracted file
    /// name, was not valid UTF-8.
    #[error("{0:?} is not valid UTF-8")]
    NotUtf8(PathBuf),

    /// A tree entry carried a name that could escape or collide inside the
    /// materialization destination — `.`, `..`, a path separator, or a
    /// duplicate of an earlier entry in the same tree. Checkout runs on
    /// the *host*, before any sandbox exists, so a crafted (fsck-invalid
    /// but storable) tree is refused before anything is written.
    #[error("refusing to materialize tree entry {name:?}: {detail}")]
    UnsafeEntry {
        /// The offending entry name.
        name: String,
        /// Why it was refused, human-readable.
        detail: String,
    },

    /// A path under a materialization destination could not be read or
    /// written.
    #[error("could not access {path}: {source}")]
    Io {
        /// The path being accessed.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// A [`crate::Component`] carried a `dest` unsafe to use as a path
    /// segment (not empty and not a single safe component), or a `url` or
    /// `sha256` unsafe to interpolate into a shell command.
    #[error("invalid toolchain component: {0}")]
    InvalidComponent(String),

    /// Running an external program (`docker`, `sprite`, `curl`, `tar`,
    /// `sha256sum`/`shasum`) failed to start at all — the readiness probes
    /// this phase ports from `pre-redo` exist precisely to turn this into
    /// an actionable message instead of a raw "os error 2".
    #[error("could not run `{program}`: {detail}")]
    Spawn {
        /// The program that could not be started.
        program: String,
        /// What failed, human-readable.
        detail: String,
    },

    /// An external program ran but reported failure (nonzero exit, or
    /// output this crate could not parse).
    #[error("{program} failed: {detail}")]
    Process {
        /// The program that failed.
        program: String,
        /// What failed, human-readable.
        detail: String,
    },

    /// A downloaded component's content did not match its recorded
    /// sha256 — refused rather than extracted anyway.
    #[error("{url}: expected sha256 {expected}, got {actual}")]
    HashMismatch {
        /// The component's source URL.
        url: String,
        /// The recorded sha256.
        expected: String,
        /// The sha256 actually computed.
        actual: String,
    },

    /// The sandbox reported an infrastructure failure rather than a
    /// completed run — never itself a `Status::Error` result
    /// (`effect.result-taxonomy`); see this type's own doc.
    #[error("the sandbox did not complete a run: {0}")]
    Sandbox(String),
}

impl From<ents_receive::Error> for Error {
    fn from(source: ents_receive::Error) -> Self {
        Self::Receive(Box::new(source))
    }
}

/// The `Result` alias every fallible `ents-effect` operation returns.
pub type Result<T> = std::result::Result<T, Error>;
