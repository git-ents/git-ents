//! `git ents agent`'s argument grammar — `figue` derive definitions only.
//!
//! Per this project's engineering conventions, this module carries no
//! logic: every doc comment here becomes `--help` text, and `git-ents`'s
//! own `exe` module would be the only place an [`AgentAction`] variant is
//! interpreted — Phase 1 stops at defining this grammar; wiring it into the
//! `git-ents` binary is CLI wiring outside this phase's scope.

use std::path::PathBuf;

use facet::Facet;
use figue as args;

/// `git ents agent` actions.
#[derive(Facet)]
#[repr(u8)]
pub enum AgentAction {
    /// Start a new agent session: a task prompt, seeded verbatim as the
    /// thread's first turn, plus the genesis-time choices that freeze into
    /// the session's metadata. The session starts in `planning`, with no
    /// plan yet.
    New {
        /// The initial task prompt.
        #[facet(args::named)]
        prompt: String,
        /// The model id the run executes against.
        #[facet(args::named)]
        model: String,
        /// Toolchains this run depends on (repeatable); each is hash-pinned
        /// to its ref's current tip at creation.
        #[facet(args::named, args::label = "NAME", default)]
        toolchain: Vec<String>,
        /// The ref the run executes against as its starting point.
        #[facet(args::named, default = "HEAD")]
        base: String,
        /// The session's initially resolved review policy. `manual` (default):
        /// no review opens on its own; you start one yourself. `auto`: a review
        /// of the result opens automatically once the run finishes.
        #[facet(args::named, default = "manual")]
        review_policy: String,
        /// The genesis oid of a prior session this one retries.
        #[facet(args::named)]
        retry_of: Option<String>,
        /// Key to sign with; defaults to `user.signingkey`.
        #[facet(args::named)]
        key: Option<PathBuf>,
    },
    /// Draft or redraft a session's plan text, committing the plan leaf and
    /// transitioning it to `ready`. Drops any existing confirm.
    Plan {
        /// The session to draft a plan for.
        #[facet(args::positional)]
        id: String,
        /// The plan text.
        #[facet(args::named)]
        text: String,
        /// Key to sign with; defaults to `user.signingkey`.
        #[facet(args::named)]
        key: Option<PathBuf>,
    },
    /// Confirm a session's current plan: binds its hash, queueing the
    /// session for execution (Phase 2).
    Confirm {
        /// The session to confirm.
        #[facet(args::positional)]
        id: String,
        /// Override the session's resolved review policy at confirm time.
        #[facet(args::named)]
        review_policy: Option<String>,
        /// Key to sign with; defaults to `user.signingkey`.
        #[facet(args::named)]
        key: Option<PathBuf>,
    },
    /// List the agent sessions recorded in this repository.
    List,
    /// Show one agent session.
    Show {
        /// The session's id.
        #[facet(args::positional)]
        id: String,
    },
}
