//! `git ents review`'s argument grammar — `figue` derive definitions only.
//!
//! Per this project's engineering conventions, this module carries no
//! logic: every doc comment here becomes `--help` text, and `git-ents`'s
//! own `exe` module is the only place a [`ReviewAction`] variant is
//! interpreted.

use std::path::PathBuf;

use facet::Facet;
use figue as args;

/// `git ents review` actions.
#[derive(Facet)]
#[repr(u8)]
pub enum ReviewAction {
    /// Review a commit: a verdict plus a body, occupying two refs — the
    /// review's own entity ref at `refs/meta/reviews/<id>`, and a retention
    /// pin at `refs/meta/pins/reviews/<id>` keeping the reviewed commit (and
    /// its ancestry) reachable.
    New {
        /// Revision to review.
        #[facet(args::named, default = "HEAD")]
        target: String,
        /// The review's verdict, e.g. approve or request-changes; custom
        /// values are schema, not a platform feature.
        #[facet(args::named)]
        verdict: String,
        /// The review's body text.
        #[facet(args::named)]
        body: String,
        /// Key to sign with; defaults to `user.signingkey`.
        #[facet(args::named)]
        key: Option<PathBuf>,
    },
    /// List the reviews recorded in this repository.
    List {
        /// Keep only reviews of this revision.
        #[facet(args::named)]
        target: Option<String>,
    },
    /// Show one review: its reviewed commit, verdict, body, and discussion
    /// thread (comments naming it as their context).
    Show {
        /// The review's id.
        #[facet(args::positional)]
        id: String,
    },
}
