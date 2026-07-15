//! The error type every `ents-anchor` operation returns.

use gix::ObjectId;

/// Everything that can go wrong capturing or projecting an [`crate::Anchor`].
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A revision string ([`crate::capture`]'s or [`crate::project`]'s
    /// `revision`/`target` argument) could not be resolved to a commit in
    /// the repository.
    #[error("could not resolve {0:?}")]
    Resolve(String),
    /// A git object could not be read or decoded.
    #[error("git object operation failed: {0}")]
    Object(String),
    /// The tree diff between the anchor commit and the target commit
    /// failed.
    #[error("tree diff failed: {0}")]
    Diff(String),
    /// The anchor names a path that is not a regular file in the commit it
    /// was captured against (`anchor.definition`'s path validation).
    #[error("no file at {path:?} in {commit}")]
    MissingPath {
        /// The commit the path was looked up in.
        commit: ObjectId,
        /// The path that is not a file there.
        path: String,
    },
    /// The line range does not fit the file it is anchored to
    /// (`anchor.definition`'s line-range validation).
    #[error("lines {start}..={end} do not fit {path:?} ({len} lines)")]
    LinesOutOfRange {
        /// The file the range was checked against.
        path: String,
        /// The 1-based first line of the range.
        start: u64,
        /// The 1-based last line of the range.
        end: u64,
        /// How many lines the file actually has.
        len: u64,
    },
    /// [`crate::project_exact`]'s anchor commit is no longer present in the
    /// repository (garbage collected) — [`crate::project`] catches this and
    /// retries with [`crate::project_from_context`]
    /// (`anchor.fuzzy-fallback`); a caller invoking
    /// [`crate::project_exact`] directly sees it as an ordinary error.
    #[error("the anchor commit {0} no longer exists")]
    AnchorCommitMissing(ObjectId),
    /// [`crate::capture_worktree`] or [`crate::project_worktree`] was asked
    /// to read the working tree of a repository that has none (a bare
    /// repository). Capture or project against a revision instead
    /// (`anchor.working-tree` applies only where a working tree exists).
    #[error("the repository has no working tree")]
    NoWorkingTree,
}

/// The `Result` alias every `ents-anchor` operation returns.
pub type Result<T> = std::result::Result<T, Error>;
