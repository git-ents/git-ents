//! `git ents comment`'s argument grammar — `figue` derive definitions
//! only.
//!
//! Per this project's engineering conventions, this module carries no
//! logic: every doc comment here becomes `--help` text, and `git-ents`'s
//! own `exe` module is the only place a [`CommentAction`] variant is
//! interpreted.

use std::path::PathBuf;

use facet::Facet;
use figue as args;

/// `git ents comment` actions.
#[derive(Facet)]
#[repr(u8)]
pub enum CommentAction {
    /// List the comments recorded in this repository.
    List,
    /// Anchor a comment to a file at a revision.
    Add {
        /// Repository-relative path of the file the comment anchors to.
        #[facet(args::positional)]
        path: String,
        /// The comment's body text.
        #[facet(args::named)]
        body: String,
        /// Lines to anchor, as `<start>[:<end>]` (1-based, inclusive);
        /// omit for a whole-file comment.
        #[facet(args::named)]
        lines: Option<String>,
        /// Revision to anchor against.
        #[facet(args::named, default = "HEAD")]
        rev: String,
        /// Key to sign with; defaults to `user.signingkey`.
        #[facet(args::named)]
        key: Option<PathBuf>,
    },
    /// Show one comment: its anchor, projected onto a revision, and its
    /// body.
    Show {
        /// The comment's id.
        #[facet(args::positional)]
        id: String,
        /// Revision to project the comment's anchor onto.
        #[facet(args::named, default = "HEAD")]
        rev: String,
    },
}
