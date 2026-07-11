//! `git ents`'s argument grammar — `figue` derive definitions only.
//!
//! Per this project's engineering conventions, this module carries no
//! logic: every doc comment here becomes `--help` text, and
//! [`crate::exe`] is the only place a [`Top`] variant is interpreted.

use std::path::PathBuf;

use facet::Facet;
use figue::{self as args, FigueBuiltins};

/// Local root wiring, subcommand surface, and the single-node hosted
/// root's git-hook plumbing (`docs/development-plan.adoc`, phase 6).
#[derive(Facet)]
pub struct Cli {
    /// The subcommand to run.
    #[facet(args::subcommand)]
    pub command: Top,
    /// `--help`/`--version`/`--completions` wiring `figue` provides for
    /// every CLI built on it.
    #[facet(flatten)]
    pub builtins: FigueBuiltins,
}

/// Every top-level `git ents` subcommand.
// @relation(roots.local, roots.worktree-update, scope=file)
#[derive(Facet)]
#[repr(u8)]
pub enum Top {
    /// Configure this repository for signed local writes: resolve or
    /// generate a signing key, record it as `user.signingkey` with
    /// `gpg.format=ssh`, and set `receive.denyCurrentBranch=updateInstead`
    /// so the integration-test harness can push into this repository's
    /// checked-out branch (`roots.worktree-update`).
    Setup {
        /// Key to sign with; defaults to `user.signingkey`, else a new
        /// `~/.ssh/id_ed25519` is generated.
        #[facet(args::named)]
        key: Option<PathBuf>,
    },
    /// Manage the repository members at `refs/meta/member/<username>`.
    Members {
        /// The member action to run.
        #[facet(args::subcommand)]
        action: MembersAction,
    },
    /// Manage this repository's account identity at `refs/meta/account`.
    Account {
        /// The account action to run.
        #[facet(args::subcommand)]
        action: AccountAction,
    },
    /// Manage the configured effects at `refs/meta/effects/<name>` and run
    /// them locally.
    Effect {
        /// The effect action to run.
        #[facet(args::subcommand)]
        action: EffectAction,
    },
    /// Manage the toolchains stored as git trees at
    /// `refs/meta/toolchains/<name>`.
    Toolchain {
        /// The toolchain action to run.
        #[facet(args::subcommand)]
        action: ToolchainAction,
    },
    /// Comment on code: one comment per ref at `refs/meta/comments/<id>`,
    /// anchored to a blob (and optionally lines) at a commit.
    Comment {
        /// The comment action to run.
        #[facet(args::subcommand)]
        action: CommentAction,
    },
    /// Work with entities awaiting adoption at
    /// `refs/meta/inbox/<member>/<id>`.
    Inbox {
        /// The inbox action to run.
        #[facet(args::subcommand)]
        action: InboxAction,
    },
    /// Record that `oid` was redacted (`refs/meta/redactions/<id>`),
    /// refusing any future push that would refill it
    /// (`receive.redaction-ingest`). Admin-only: the gate's default
    /// namespace-authorization arm requires admin-registered provenance
    /// for `refs/meta/redactions/*`.
    Redact {
        /// The object id to redact.
        #[facet(args::positional)]
        oid: String,
        /// A human-readable reason recorded alongside the redaction.
        #[facet(args::named)]
        reason: String,
        /// Key to sign with; defaults to `user.signingkey`.
        #[facet(args::named)]
        key: Option<PathBuf>,
    },
    /// Plumbing invoked by git's own hooks on the single-node hosted root
    /// (`git.ents.cloud`) — not part of the porcelain surface a developer
    /// runs directly.
    Hook {
        /// Which hook is running.
        #[facet(args::subcommand)]
        action: HookAction,
    },
}

/// `git ents members` actions.
#[derive(Facet)]
#[repr(u8)]
pub enum MembersAction {
    /// List the members recorded in this repository.
    List,
    /// Enroll a new member, or update an existing one's key.
    Add {
        /// The member's username (`refs/meta/member/<username>`).
        #[facet(args::positional)]
        username: String,
        /// The public key to enroll (an OpenSSH single-line public key);
        /// defaults to the signer's own public key.
        #[facet(args::named)]
        pubkey: Option<String>,
        /// Key to sign the enrollment with; defaults to `user.signingkey`.
        #[facet(args::named)]
        key: Option<PathBuf>,
    },
    /// Remove a member, deleting its ref.
    Remove {
        /// The member (username) to remove.
        #[facet(args::positional)]
        username: String,
        /// Key to sign the removal with; defaults to `user.signingkey`.
        #[facet(args::named)]
        key: Option<PathBuf>,
    },
    /// Revoke a member's key (`model.member-revocation`): the record
    /// stays, but the key no longer authorizes new signatures.
    Revoke {
        /// The member (username) to revoke.
        #[facet(args::positional)]
        username: String,
        /// Key to sign the revocation with; defaults to `user.signingkey`.
        #[facet(args::named)]
        key: Option<PathBuf>,
    },
    /// Lift a revocation, restoring a member's key to active.
    Unrevoke {
        /// The member (username) to unrevoke.
        #[facet(args::positional)]
        username: String,
        /// Key to sign the unrevocation with; defaults to
        /// `user.signingkey`.
        #[facet(args::named)]
        key: Option<PathBuf>,
    },
    /// Report whether a key is an active member.
    Check {
        /// Key to look for; defaults to `user.signingkey`.
        #[facet(args::named)]
        key: Option<PathBuf>,
    },
}

/// `git ents account` actions.
#[derive(Facet)]
#[repr(u8)]
pub enum AccountAction {
    /// Create or update this repository's account identity.
    Create {
        /// The member this account belongs to; defaults to the signer's
        /// own member (resolved by public key).
        #[facet(args::named)]
        member: Option<String>,
        /// The login identity the member authenticates as.
        #[facet(args::named)]
        login: String,
        /// Key to sign with; defaults to `user.signingkey`.
        #[facet(args::named)]
        key: Option<PathBuf>,
    },
}

/// `git ents effect` actions.
#[derive(Facet)]
#[repr(u8)]
pub enum EffectAction {
    /// List the effects configured in this repository.
    List,
    /// Show one effect's definition and, when a commit is given, its
    /// result.
    Show {
        /// The effect's name.
        #[facet(args::positional)]
        name: String,
        /// Commit to show the result for.
        #[facet(args::named)]
        at: Option<String>,
    },
    /// Define (or replace) an effect and push the update.
    Add {
        /// Name to record the effect under (`refs/meta/effects/<name>`).
        #[facet(args::positional)]
        name: String,
        /// The query this effect triggers on (`query.grammar`).
        #[facet(args::named)]
        on: String,
        /// The command the effect runs.
        #[facet(args::positional)]
        run: String,
        /// Toolchain (`refs/meta/toolchains/<name>`) to activate before
        /// the command runs (repeatable).
        #[facet(args::named, args::label = "TOOLCHAIN", default)]
        toolchain: Vec<String>,
        /// Key to sign with; defaults to `user.signingkey`.
        #[facet(args::named)]
        key: Option<PathBuf>,
    },
    /// Run this repository's effects locally against every commit still
    /// owed a result, or a single one with `--at`
    /// (`effect.local-run`): identical toolchain materialization and
    /// sandbox path to a hosted worker, the queue skipped entirely.
    Run {
        /// The effect's name.
        #[facet(args::positional)]
        name: String,
        /// Commit to run against; omit to run every outstanding commit
        /// (`query.workset`).
        #[facet(args::named)]
        at: Option<String>,
        /// Key to sign the result with; defaults to `user.signingkey`.
        #[facet(args::named)]
        key: Option<PathBuf>,
    },
    /// Show recorded results for an effect, newest first.
    Log {
        /// The effect's name.
        #[facet(args::positional)]
        name: String,
    },
}

/// `git ents toolchain` actions.
#[derive(Facet)]
#[repr(u8)]
pub enum ToolchainAction {
    /// Import a local directory as toolchain `name`, embedding its
    /// contents whole (`ents_effect::Recipe::Embedded`).
    Import {
        /// Name to record the toolchain under
        /// (`refs/meta/toolchains/<name>`).
        #[facet(args::positional)]
        name: String,
        /// Directory of executables to import, activated on `PATH` when
        /// an effect declares this toolchain.
        #[facet(args::positional)]
        bin: PathBuf,
        /// Key to sign with; defaults to `user.signingkey`.
        #[facet(args::named)]
        key: Option<PathBuf>,
    },
    /// Show a toolchain's provenance.
    View {
        /// Name (`refs/meta/toolchains/<name>`) to view.
        #[facet(args::positional)]
        name: String,
    },
    /// Show a toolchain's import history — the ref's own commit log.
    Log {
        /// Name (`refs/meta/toolchains/<name>`) to show history for.
        #[facet(args::positional)]
        name: String,
    },
}

/// `git ents comment` actions.
#[derive(Facet)]
#[repr(u8)]
pub enum CommentAction {
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

/// `git ents inbox` actions.
#[derive(Facet)]
#[repr(u8)]
pub enum InboxAction {
    /// List entities awaiting adoption.
    List,
    /// Adopt an inbox entity onto its canonical ref
    /// (`sync.adoption-machinery`): a merge that keeps the author's
    /// original signed commit in ancestry
    /// (`sync.adoption-no-cherry-pick`).
    Adopt {
        /// The inbox entry to adopt, as `<member>/<id>`.
        #[facet(args::positional)]
        entry: String,
        /// Key to sign the adoption merge with; defaults to
        /// `user.signingkey`.
        #[facet(args::named)]
        key: Option<PathBuf>,
    },
}

/// Plumbing subcommands the single-node hosted root's git hooks invoke;
/// see `crate::hook`'s own doc for what each does and why.
#[derive(Facet)]
#[repr(u8)]
pub enum HookAction {
    /// Run as git's own `pre-receive` hook: evaluate the gate against
    /// every proposed transition read from stdin, refusing the whole
    /// push under the mandatory gate if any fails.
    PreReceive,
    /// Run as git's own `post-receive` hook: reconcile outstanding effect
    /// obligations (`receive.reconstructible`) and run them.
    PostReceive,
    /// Reconcile outstanding effect obligations without running anything
    /// — the boot-time scan on its own, for operational use and testing.
    Reconcile,
}
