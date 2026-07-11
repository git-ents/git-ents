//! The porcelain-wide error type: every subcommand's failure, rendered for
//! a terminal.

use std::path::PathBuf;

/// Every way a `git-ents` subcommand can fail.
///
/// Each variant documents when it occurs and what the user should do —
/// this is the only layer that renders a failure for a human, so the
/// detail belongs here rather than in a library crate's own error type.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The current directory is not inside a git repository, or the
    /// discovered repository has no `.git` directory `git-ents` can open.
    /// Run the command inside a git repository.
    #[error("not a git repository (or any parent up to mount point): {path}")]
    NotARepo {
        /// The directory `git-ents` started looking from.
        path: PathBuf,
    },

    /// No signing key could be resolved: `--key` was not given,
    /// `user.signingkey` is unset, and no default key exists at
    /// `~/.ssh/id_ed25519`. Run `git ents setup` first.
    #[error("no signing key configured; run `git ents setup` or pass --key")]
    NoSigningKey,

    /// The signing key at `path` could not be read as an OpenSSH private
    /// key, or is passphrase-protected (unsupported in this phase: use an
    /// unencrypted key, or load one via `ssh-agent` in a future phase).
    #[error("cannot use signing key at {path}: {detail}")]
    BadSigningKey {
        /// The key file that failed to load.
        path: PathBuf,
        /// What went wrong.
        detail: String,
    },

    /// The gate refused the proposed mutation (`gate.verdict-reason`): the
    /// refusal's own rendering names the rule and offers the inbox
    /// alternative when one applies.
    #[error("rejected: {0}")]
    Refused(String),

    /// `receive` rejected the batch as a stale compare-and-swap: another
    /// writer moved a ref between read and write. Retry the command.
    #[error("rejected: {name} changed concurrently, retry")]
    Stale {
        /// The ref whose precondition was stale.
        name: String,
    },

    /// A previously redacted object would have been refilled by this
    /// mutation (`receive.redaction-ingest`); the mutation is refused.
    #[error("refused: object {oid} was redacted and cannot be refilled")]
    Redacted {
        /// The redacted object id.
        oid: gix_hash::ObjectId,
    },

    /// The named entity (member, effect, toolchain, comment, inbox entry)
    /// does not exist.
    #[error("not found: {what}")]
    NotFound {
        /// What was being looked up.
        what: String,
    },

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

    /// A malformed command-line argument that passed `figue`'s own parsing
    /// but fails a semantic check this crate makes (an invalid line range,
    /// an unparsable oid, ...).
    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    /// Opening or reading the local git repository failed. Boxed (like the
    /// other large variants below): `gix::open::Error` is large enough on
    /// its own to trip `clippy::result_large_err` for every fallible
    /// function in this crate if stored inline, the same reasoning
    /// `ents-effect`'s own error type documents for its boxed
    /// `ents_receive::Error` variant.
    #[error(transparent)]
    Repo(Box<gix::open::Error>),

    /// A `gix-ref-store` failure: reading or writing a ref.
    #[error(transparent)]
    Refs(#[from] gix_ref_store::Error),

    /// An `ents-gate` failure: the gate itself could not reach a verdict
    /// (a store or object read failed), distinct from a reached refusal.
    #[error(transparent)]
    Gate(#[from] ents_gate::Error),

    /// An `ents-receive` failure: `receive` itself could not reach an
    /// outcome. Boxed; see [`Error::Repo`]'s own doc.
    #[error(transparent)]
    Receive(Box<ents_receive::Error>),

    /// An `ents-effect` failure: toolchain resolution, materialization, or
    /// the executor itself. Boxed; see [`Error::Repo`]'s own doc.
    #[error(transparent)]
    Effect(Box<ents_effect::Error>),

    /// An `ents-anchor` failure: capturing or projecting a code anchor.
    #[error(transparent)]
    Anchor(#[from] ents_anchor::Error),

    /// An `ents-sync` failure: pre-flight, routing, or merge. Boxed; see
    /// [`Error::Repo`]'s own doc.
    #[error(transparent)]
    Sync(Box<ents_sync::Error>),

    /// An `ents-model` failure: building or validating a refname or typed
    /// tree.
    #[error(transparent)]
    Model(#[from] ents_model::Error),

    /// A `facet-git-tree` (de)serialization failure.
    #[error(transparent)]
    Tree(#[from] facet_git_tree::Error),

    /// A raw object-store write failed (building a toolchain import's tree,
    /// or a mutation commit).
    #[error(transparent)]
    ObjectWrite(#[from] gix_object::write::Error),
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

impl From<ents_effect::Error> for Error {
    fn from(source: ents_effect::Error) -> Self {
        Self::Effect(Box::new(source))
    }
}

impl From<ents_sync::Error> for Error {
    fn from(source: ents_sync::Error) -> Self {
        Self::Sync(Box::new(source))
    }
}

/// This crate's `Result` alias.
pub type Result<T> = std::result::Result<T, Error>;
