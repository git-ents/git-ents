//! `git ents comment`'s argument grammar — `figue` derive definitions
//! only.
//!
//! Per this project's engineering conventions, this module carries no
//! logic: every doc comment here becomes `--help` text, and `git-ents`'s
//! own `exe` module is the only place a [`CommentAction`] variant is
//! interpreted.

use std::path::PathBuf;

use ents_attrs as ents;
use facet::Facet;
use figue as args;

/// `git ents comment` actions.
#[derive(Facet)]
#[repr(u8)]
pub enum CommentAction {
    /// List the comments recorded in this repository, each anchor
    /// projected onto HEAD (or, with --worktree, onto the working tree).
    ///
    /// With --porcelain, emits a stable machine-readable form:
    /// blank-line-separated records, each starting with the line
    /// `<id> <state> <projection> <location>` — projection is current,
    /// relocated, outdated, or deleted ("-" for a comment with no
    /// anchor); location is `path:start-end`, `path` for a whole-file
    /// anchor ("-" when there is no anchor or the file is gone) —
    /// followed by optional `context <c>` and `parent <id>` lines, then
    /// the body with every line prefixed by one tab.
    List {
        /// Project each anchor onto the working tree's on-disk bytes
        /// instead of HEAD.
        #[facet(args::named, default)]
        worktree: bool,
        /// Keep only comments in this state (e.g. open, resolved).
        #[facet(args::named)]
        state: Option<String>,
        /// Shorthand for --state open.
        #[facet(args::named, default)]
        open: bool,
        /// Keep only comments naming this context (a ref path below
        /// refs/meta/, e.g. `issues/<id>`).
        #[facet(args::named)]
        context: Option<String>,
        /// Emit the stable machine-readable form described above.
        #[facet(args::named, default)]
        porcelain: bool,
    },
    /// Create a comment about something: anchor it to a file (at a
    /// revision or in the working tree), name a context entity, reply to
    /// a parent comment, or any combination. A comment about none of
    /// these is refused.
    Add {
        /// Repository-relative path of the file the comment anchors to;
        /// omit for a comment about a context or parent only.
        #[facet(args::positional, default)]
        path: Option<String>,
        /// The comment's body text; omit to compose it in
        /// $GIT_EDITOR/$EDITOR instead (lines starting with '#' are
        /// stripped, and an empty body aborts the command).
        #[facet(args::named, ents::compose)]
        body: Option<String>,
        /// Lines to anchor, as `<start>[:<end>]` (1-based, inclusive);
        /// omit for a whole-file comment.
        #[facet(args::named)]
        lines: Option<String>,
        /// Revision to anchor against.
        #[facet(args::named, default = "HEAD")]
        rev: String,
        /// Anchor against the working tree's current on-disk bytes
        /// instead of --rev.
        #[facet(args::named, default)]
        worktree: bool,
        /// Canonical ref path below refs/meta/ of the entity this comment
        /// belongs to, e.g. `issues/<id>` or `reviews/<target>/<member>`.
        #[facet(args::named)]
        context: Option<String>,
        /// Id of the comment this one replies to.
        #[facet(args::named)]
        parent: Option<String>,
        /// Key to sign with; defaults to `user.signingkey`.
        #[facet(args::named)]
        key: Option<PathBuf>,
    },
    /// Reply to a comment: a new comment whose parent is the given id,
    /// inheriting its aboutness from the thread — no anchor or context
    /// required.
    Reply {
        /// The comment being replied to.
        #[facet(args::positional)]
        id: String,
        /// The reply's body text.
        #[facet(args::named)]
        body: String,
        /// Key to sign with; defaults to `user.signingkey`.
        #[facet(args::named)]
        key: Option<PathBuf>,
    },
    /// Mark a comment resolved: an ordinary mutation commit on the
    /// comment's own ref, never a deletion.
    Resolve {
        /// The comment to resolve.
        #[facet(args::positional)]
        id: String,
        /// Key to sign with; defaults to `user.signingkey`.
        #[facet(args::named)]
        key: Option<PathBuf>,
    },
    /// Reopen a resolved comment, the same way resolve marks it.
    Reopen {
        /// The comment to reopen.
        #[facet(args::positional)]
        id: String,
        /// Key to sign with; defaults to `user.signingkey`.
        #[facet(args::named)]
        key: Option<PathBuf>,
    },
    /// Show one comment: its state, context, parent, body, and — when
    /// anchored — its anchor projected onto a revision or the working
    /// tree.
    Show {
        /// The comment's id.
        #[facet(args::positional)]
        id: String,
        /// Revision to project the comment's anchor onto.
        #[facet(args::named, default = "HEAD")]
        rev: String,
        /// Project onto the working tree's on-disk bytes instead of
        /// --rev.
        #[facet(args::named, default)]
        worktree: bool,
    },
}
