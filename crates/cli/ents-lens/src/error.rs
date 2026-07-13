//! The lens's error type: every failure a request handler can hit while
//! reading `refs/meta/*`, projecting an anchor, or writing a new comment.

/// A lens operation's result.
pub type Result<T> = std::result::Result<T, Error>;

/// Everything that can go wrong deriving a lens response or composing a
/// comment through it.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A comment read, listing, projection, or mutation failed in the
    /// shared `ents-forge` library the lens calls (`lens.parity`). The
    /// caller should surface the message; it is never a protocol-level
    /// fault.
    #[error(transparent)]
    Forge(#[from] ents_forge::Error),

    /// The repository could not be opened at the injected path — the lens
    /// was wired against a directory that is not a git working tree.
    #[error("open repository: {0}")]
    Repo(String),

    /// A filesystem operation on the compose template under `.git/`
    /// (`lens.compose`) failed. The caller should report it; the comment
    /// was not created.
    #[error("template {path}: {source}")]
    Template {
        /// The template path the operation targeted.
        path: std::path::PathBuf,
        /// The underlying IO error.
        source: std::io::Error,
    },

    /// An `executeCommand` request named a command the lens exposes but
    /// carried the wrong arguments (a missing or non-string comment id, for
    /// instance). The caller sent a malformed request.
    #[error("bad command arguments: {0}")]
    BadArguments(String),
}

impl From<gix::open::Error> for Error {
    fn from(source: gix::open::Error) -> Self {
        Self::Repo(source.to_string())
    }
}
