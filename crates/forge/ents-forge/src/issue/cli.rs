//! `git ents issue`'s argument grammar — `figue` derive definitions only.
//!
//! Per this project's engineering conventions, this module carries no
//! logic: every doc comment here becomes `--help` text, and `git-ents`'s
//! own `exe` module is the only place an [`IssueAction`] variant is
//! interpreted.

use std::path::PathBuf;

use ents_attrs as ents;
use facet::Facet;
use figue as args;

/// `git ents issue` actions.
#[derive(Facet)]
#[repr(u8)]
pub enum IssueAction {
    /// List the issues recorded in this repository.
    ///
    /// With --porcelain, emits a stable machine-readable form:
    /// blank-line-separated records, each starting with the line
    /// `<id> <state>` (the full id, never abbreviated), followed by a
    /// `title <title>` line, `assignees <a, b>` and `labels <x, y>` lines
    /// when non-empty, then the body with every line prefixed by one tab.
    List {
        /// Emit the stable machine-readable form described above.
        #[facet(args::named, default)]
        porcelain: bool,
    },
    /// Show one issue.
    Show {
        /// The issue's id.
        #[facet(args::positional)]
        id: String,
    },
    /// Create an issue. With --title omitted, opens $GIT_EDITOR/$EDITOR on
    /// a scratch file: its first line becomes the title, the remaining
    /// lines become the body, lines starting with '#' are stripped, and
    /// leaving the title empty aborts the command.
    New {
        /// The issue's title; omit to compose it (and the body) in an
        /// editor instead.
        #[facet(args::named, ents::compose)]
        title: Option<String>,
        /// The issue's body; ignored (and instead composed in the editor)
        /// when --title is omitted.
        #[facet(args::named, ents::compose)]
        body: Option<String>,
        /// The issue's initial state.
        #[facet(args::named, default = "open")]
        state: String,
        /// Labels to attach (repeatable).
        #[facet(args::named, args::label = "LABEL", default)]
        label: Vec<String>,
        /// Members to assign (repeatable).
        #[facet(args::named, args::label = "USERNAME", default)]
        assignee: Vec<String>,
        /// Key to sign with; defaults to `user.signingkey`.
        #[facet(args::named)]
        key: Option<PathBuf>,
    },
    /// Edit an existing issue's state, assignees, and/or labels.
    /// Assignees/labels replace the previous set entirely when given at
    /// least one value; omit an option to leave that field unchanged.
    Edit {
        /// The issue to edit.
        #[facet(args::positional)]
        id: String,
        /// Replace the issue's state.
        #[facet(args::named)]
        state: Option<String>,
        /// Replace the issue's labels (repeatable; give at least one to
        /// replace the set).
        #[facet(args::named, args::label = "LABEL", default)]
        label: Vec<String>,
        /// Replace the issue's assignees (repeatable; give at least one to
        /// replace the set).
        #[facet(args::named, args::label = "USERNAME", default)]
        assignee: Vec<String>,
        /// Key to sign with; defaults to `user.signingkey`.
        #[facet(args::named)]
        key: Option<PathBuf>,
    },
}
