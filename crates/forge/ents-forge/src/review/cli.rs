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
    /// review's own entity ref at `refs/meta/reviews/<target>/<member>`, and
    /// a retention pin at `refs/meta/pins/reviews/<target>/<member>` keeping
    /// the reviewed commit (and its ancestry) reachable. Re-reviewing after
    /// the target moves advances the same two refs fast-forward rather than
    /// minting new ones.
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
    /// Withdraw this member's own review: writes a new `Withdrawn`-state
    /// entity onto the *same* two refs the original review occupies,
    /// preserving its verdict and body — append-only, so the prior verdict
    /// stays in the ref's history. Refuses when this member has no
    /// existing review reaching `target`.
    Withdraw {
        /// Revision identifying the review to withdraw: resolved exactly
        /// as `new`'s own target and re-review advance are, so a
        /// descendant of the reviewed commit still finds it.
        #[facet(args::named, default = "HEAD")]
        target: String,
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
        /// The review's genesis target segment (`refs/meta/reviews/<target>/*`).
        #[facet(args::positional)]
        target: String,
        /// The reviewer's member id (`refs/meta/reviews/*/<member>`).
        #[facet(args::positional)]
        member: String,
    },
}
