//! `git ents`'s argument grammar — `figue` derive definitions only.
//!
//! Per this project's engineering conventions, this module carries no
//! logic: every doc comment here becomes `--help` text, and
//! [`crate::exe`] is the only place a [`Top`] variant is interpreted.

use std::path::PathBuf;

use facet::Facet;
use figue::{self as args, FigueBuiltins};

pub use ents_forge::comment::CommentAction;
pub use ents_forge::issue::IssueAction;
pub use ents_forge::review::ReviewAction;
pub use ents_kiln::toolchain::ToolchainAction;

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
// @relation(roots.local, roots.worktree-update, roots.single-node-hosted, lens.serve, scope=file)
#[derive(Facet)]
#[repr(u8)]
pub enum Top {
    /// Configure this repository for signed local writes: resolve or
    /// generate a signing key, record it as `user.signingkey` with
    /// `gpg.format=ssh`, and set `receive.denyCurrentBranch=updateInstead`
    /// so the integration-test harness can push into this repository's
    /// checked-out branch (`roots.worktree-update`).
    ///
    /// With `--hosted`, configures the single-node hosted root instead
    /// (`roots.single-node-hosted`): a signing key for the hosted worker,
    /// and this binary's own `pre-receive`/`post-receive` hooks installed
    /// into a bare repository's `hooks/` directory. Without these hooks
    /// installed, a hosted bare repository accepts every push ungated —
    /// stock git's `receive-pack` has no gate of its own.
    Setup {
        /// Key to sign with; defaults to `user.signingkey`, else a new
        /// `~/.ssh/id_ed25519` is generated.
        #[facet(args::named)]
        key: Option<PathBuf>,
        /// Configure the single-node hosted root instead of the local
        /// one: install this binary's `hook pre-receive`/`hook
        /// post-receive` into a bare repository's own hooks, and a
        /// signing key for the hosted worker.
        #[facet(args::named, default)]
        hosted: bool,
        /// The bare repository to configure with `--hosted`; defaults to
        /// the current directory. Ignored without `--hosted`.
        #[facet(args::positional, default)]
        path: Option<PathBuf>,
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
    /// Manage issues at `refs/meta/issues/<id>`.
    Issue {
        /// The issue action to run.
        #[facet(args::subcommand)]
        action: IssueAction,
    },
    /// Review a commit: a verdict plus a body at `refs/meta/reviews/<id>`,
    /// with a retention pin at `refs/meta/pins/reviews/<id>` keeping the
    /// reviewed commit reachable.
    Review {
        /// The review action to run.
        #[facet(args::subcommand)]
        action: ReviewAction,
    },
    /// Work with entities awaiting adoption at
    /// `refs/meta/inbox/<member>/<id>`.
    Inbox {
        /// The inbox action to run.
        #[facet(args::subcommand)]
        action: InboxAction,
    },
    /// Manage redactions recorded at `refs/meta/redactions/<id>`.
    Redact {
        /// The redaction action to run.
        #[facet(args::subcommand)]
        action: RedactAction,
    },
    /// Plumbing invoked by git's own hooks on the single-node hosted root
    /// (`git.ents.cloud`) — not part of the porcelain surface a developer
    /// runs directly.
    Hook {
        /// Which hook is running.
        #[facet(args::subcommand)]
        action: HookAction,
    },
    /// Start the local web UI (`roots.local`): reuses this repository's
    /// existing local composition root (the same loose-ref `RefStore`,
    /// odb, null `EventSink`, and advisory gate `git ents members`,
    /// `git ents comment`, and every other porcelain command already use)
    /// and adds only the `ents-web` HTTP frontend, bound to loopback —
    /// never git's own smart-HTTP transport, which this command does not
    /// expose in any form.
    Serve {
        /// Port to bind on loopback (`127.0.0.1`); `0` picks any free
        /// port. Defaults to 4880.
        #[facet(args::named)]
        port: Option<u16>,
        /// Key to sign web edits with; defaults to `user.signingkey`.
        #[facet(args::named)]
        key: Option<PathBuf>,
    },
    /// Serve the editor lens (`lens.serve`): a Language Server Protocol
    /// server over stdin/stdout that projects this repository's comments
    /// (`refs/meta/comments/*`) into whatever buffer an editor has open,
    /// and composes new ones through the same signed path `git ents
    /// comment` uses (`lens.parity`).
    ///
    /// Speaks LSP over stdio only: it binds no network socket and adds no
    /// git-serving transport. It reuses the very same local composition
    /// root `git ents serve` and every other porcelain command use (the
    /// same loose-ref `RefStore`, odb, null `EventSink`, and advisory
    /// gate), adding only the LSP frontend and signing with the user's own
    /// key. Meant to be launched by an editor extension (e.g. `ents-zed`),
    /// not run interactively.
    Lsp {
        /// Key to sign composed comments with; defaults to
        /// `user.signingkey`.
        #[facet(args::named)]
        key: Option<PathBuf>,
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
    /// Show this repository's account identity.
    Show,
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

/// `git ents redact` actions.
#[derive(Facet)]
#[repr(u8)]
pub enum RedactAction {
    /// List the redactions recorded in this repository.
    List,
    /// Record that `oid` was redacted (`refs/meta/redactions/<id>`),
    /// refusing any future push that would refill it
    /// (`receive.redaction-ingest`). Admin-only: the gate's default
    /// namespace-authorization arm requires admin-registered provenance
    /// for `refs/meta/redactions/*`.
    Add {
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
