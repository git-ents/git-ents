//! `git ents` — the git-ents command-line porcelain.
//!
//! It carries `git ents members` for managing the repository members recorded
//! one-ref-per-person at `refs/meta/member/<username>`, `git ents account` for
//! the account identity at `refs/meta/account`, `git ents checks` for the check
//! set, `git ents comment` for the code comments at `refs/meta/comments/<id>`,
//! and the client setup that produces the signed pushes the server
//! requires. The member commands read and write a remote's set by fetching the
//! `refs/meta/member/*` refs into the local repository, editing them through
//! [`git_ents::members`], and pushing them back.

mod debug_session;
mod interactive;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use facet::Facet;
use figue::{self as args, FigueBuiltins};
use git_anchor::{LineRange, Projection};
use git_comment::{COMMENTS_NS, Comment};
use git_ents::account::{self, Account};
use git_ents::checks::{self, CHECKS_REF, Check};
use git_ents::members::{self, MEMBER_NS, Member, Trust, member_ref};
use git_ents::revocations::{self, REVOKED_REF, Revocation};

/// Helpful guardians of your git trees.
#[derive(Facet)]
struct Cli {
    /// Remote whose refs to operate on.
    #[facet(args::named, args::short = 'r', default = "origin")]
    remote: String,
    #[facet(args::subcommand)]
    command: Top,
    #[facet(flatten)]
    builtins: FigueBuiltins,
}

#[derive(Facet)]
#[repr(u8)]
enum Top {
    /// Manage the repository members at `refs/meta/member/<username>`.
    Members {
        #[facet(args::subcommand)]
        action: Action,
    },
    /// Manage this repository's account identity at `refs/meta/account`.
    Account {
        #[facet(args::subcommand)]
        action: AccountAction,
    },
    /// Manage the configured checks at `refs/meta/checks`.
    Checks {
        #[facet(args::subcommand)]
        action: ChecksAction,
    },
    /// Comment on code: one comment per ref at `refs/meta/comments/<id>`,
    /// anchored to a blob (and optionally lines) at a commit.
    Comment {
        #[facet(args::subcommand)]
        action: CommentAction,
    },
    /// Sign in to a remote's server the same way the web UI does — sign a
    /// server-issued challenge with your key — so this machine can also open a
    /// debug session (`checks debug`).
    Login {
        /// Key to sign in with; defaults to `user.signingkey`.
        #[facet(args::named)]
        key: Option<PathBuf>,
    },
}

#[derive(Facet)]
#[repr(u8)]
enum Action {
    /// Set this machine up to sign the pushes the server requires.
    Setup {
        /// Key to sign with; defaults to `user.signingkey`, else a new or
        /// existing `~/.ssh/id_ed25519`.
        #[facet(args::named)]
        key: Option<PathBuf>,
        /// Write to this repository's config instead of your global config.
        #[facet(args::named, default)]
        local: bool,
    },
    /// List the members on a remote.
    List,
    /// Authorize a key for a member on a remote and push the update. Prompts
    /// for any field left unset when run at an interactive terminal.
    Add {
        /// Member (username) to authorize the key under — its
        /// `refs/meta/member/<username>` ref.
        #[facet(args::positional, default)]
        username: Option<String>,
        /// Key to authorize; defaults to `user.signingkey`.
        #[facet(args::named)]
        key: Option<PathBuf>,
        /// Pin a certificate authority public key instead of leaf keys: trust
        /// any certificate it issues for the member, within the cert's
        /// validity. Conflicts with `--key`.
        #[facet(args::named, args::label = "CA_PUBKEY")]
        cert_authority: Option<PathBuf>,
        /// Trust the member only at or after this OpenSSH timestamp
        /// (`YYYYMMDD[Z]` or `YYYYMMDDHHMM[SS][Z]`; append `Z` for UTC).
        #[facet(args::named, args::label = "TIMESTAMP")]
        valid_after: Option<String>,
        /// Stop trusting the member after this OpenSSH timestamp; omit for trust
        /// that never lapses on its own.
        #[facet(args::named, args::label = "TIMESTAMP")]
        valid_before: Option<String>,
        /// Link this member to an account by its genesis hash (`git ents
        /// account create` prints one).
        #[facet(args::named, args::label = "GENESIS_HASH")]
        account: Option<String>,
    },
    /// Remove a member, deleting its ref on a remote and pushing the update.
    Remove {
        /// Member (username) to remove — its `refs/meta/member/<username>` ref.
        #[facet(args::positional)]
        username: String,
    },
    /// Revoke a key fast: add its fingerprint to the `refs/meta/revoked` deny
    /// list so it is refused before its window expires, and push the update.
    Revoke {
        /// Fingerprint of the key to deny (as shown by `members list`).
        #[facet(args::positional)]
        fingerprint: String,
        /// Free-text reason recorded alongside the revocation.
        #[facet(args::named, default = "")]
        reason: String,
    },
    /// Lift a revocation, removing a fingerprint from the `refs/meta/revoked`
    /// deny list and pushing the update.
    Unrevoke {
        /// Fingerprint to stop denying.
        #[facet(args::positional)]
        fingerprint: String,
    },
    /// Report whether a key is a member and the client is configured.
    Check {
        /// Key to look for; defaults to `user.signingkey`.
        #[facet(args::named)]
        key: Option<PathBuf>,
    },
}

#[derive(Facet)]
#[repr(u8)]
enum AccountAction {
    /// Create or update this repository's account identity and push it. The
    /// presence of `refs/meta/account` is what marks the repo as an account.
    /// Prompts for any field left unset when run at an interactive terminal.
    Create {
        /// The account username — by convention the `user/<username>` repo name.
        #[facet(args::positional, default)]
        username: Option<String>,
        /// Human-facing display name; defaults to the username.
        #[facet(args::named)]
        display_name: Option<String>,
        /// Short free-text bio.
        #[facet(args::named)]
        bio: Option<String>,
    },
}

#[derive(Facet)]
#[repr(u8)]
enum ChecksAction {
    /// List the checks configured on a remote.
    List,
    /// Add (or replace) a check on a remote's set and push the update.
    /// Prompts for any field left unset when run at an interactive terminal.
    Add {
        /// Name to record the check under (`checks/<name>`).
        #[facet(args::positional, default)]
        name: Option<String>,
        /// Command the check runs (e.g. `cargo fmt --check`); omit for a
        /// composite check that only aggregates its dependencies.
        #[facet(args::positional, default)]
        command: Option<String>,
        /// Sandbox image the command runs in (reserved: the Sprite sandbox
        /// does not honor an image yet, so setting one is rejected).
        #[facet(args::named)]
        image: Option<String>,
        /// Check that must pass before this one runs (repeatable).
        #[facet(args::named, args::label = "CHECK", default)]
        depends: Vec<String>,
    },
    /// Remove a check from a remote's set and push the update.
    Remove {
        /// Name (`checks/<name>`) to drop.
        #[facet(args::positional)]
        name: String,
    },
    /// Open an interactive, read-write shell in `remote`'s persistent checks
    /// Sprite — the same sandbox its check runs execute in. Requires
    /// `git ents login <remote>` first.
    Debug,
    /// Show recorded check runs (queued/running/pass/fail/error) from
    /// `refs/meta/runs/*` on a remote, newest first.
    Runs,
}

#[derive(Facet)]
#[repr(u8)]
enum CommentAction {
    /// Anchor a comment to a file at a revision and push it. Prompts for the
    /// path and body when left unset at an interactive terminal.
    Add {
        /// Repository-relative path of the file the comment anchors to.
        #[facet(args::positional, default)]
        path: Option<String>,
        /// The comment's body text.
        #[facet(args::named)]
        body: Option<String>,
        /// Lines to anchor, as `<start>[:<end>]` (1-based, inclusive); omit
        /// for a whole-file comment.
        #[facet(args::named)]
        lines: Option<String>,
        /// Revision to anchor against.
        #[facet(args::named, default = "HEAD")]
        rev: String,
        /// Genesis id of the issue the comment belongs to.
        #[facet(args::named)]
        issue: Option<String>,
    },
    /// List the comments on a remote, each projected onto a revision.
    List {
        /// Revision to project each comment's anchor onto.
        #[facet(args::named, default = "HEAD")]
        rev: String,
    },
    /// Show one comment: author, anchor, projection, anchored text, and body.
    Show {
        /// The comment's id (or a unique prefix of it).
        #[facet(args::positional)]
        id: String,
        /// Revision to project the comment's anchor onto.
        #[facet(args::named, default = "HEAD")]
        rev: String,
    },
    /// Remove a comment, deleting its ref on a remote.
    Remove {
        /// The comment's id (or a unique prefix of it).
        #[facet(args::positional)]
        id: String,
    },
}

fn main() -> ExitCode {
    let config = match figue::builder::<Cli>() {
        Ok(builder) => builder,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    }
    .cli(|cli| cli.args(std::env::args().skip(1)))
    .help(|help| {
        help.program_name("git-ents")
            .version(env!("CARGO_PKG_VERSION"))
    })
    .build();
    let cli: Cli = match figue::Driver::new(config).run().into_result() {
        Ok(output) => output.get(),
        Err(figue::DriverError::Help {
            text,
            suggestion: suggestion @ Some(_),
        }) => {
            println!("{text}");
            if let Some(s) = suggestion {
                println!("{}", s.render_pretty());
            }
            return ExitCode::FAILURE;
        }
        Err(error) => figue::DriverOutcome::<Cli>::err(error).unwrap(),
    };
    let remote = cli.remote;
    let result = match cli.command {
        Top::Members { action } => run_members(action, &remote),
        Top::Account { action } => run_account(action, &remote),
        Top::Checks { action } => run_checks(action, &remote),
        Top::Comment { action } => run_comment(action, &remote),
        Top::Login { key } => login(&remote, key.as_deref()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run_members(action: Action, remote: &str) -> Result<(), String> {
    match action {
        Action::Setup { key, local } => setup(key.as_deref(), local),
        Action::List => members_list(remote),
        Action::Add {
            username,
            key,
            cert_authority,
            valid_after,
            valid_before,
            account,
        } => members_add(
            username,
            remote,
            key,
            cert_authority,
            valid_after,
            valid_before,
            account,
        ),
        Action::Remove { username } => members_remove(&username, remote),
        Action::Revoke {
            fingerprint,
            reason,
        } => members_revoke(&fingerprint, remote, reason),
        Action::Unrevoke { fingerprint } => members_unrevoke(&fingerprint, remote),
        Action::Check { key } => check(remote, key.as_deref()),
    }
}

fn run_account(action: AccountAction, remote: &str) -> Result<(), String> {
    match action {
        AccountAction::Create {
            username,
            display_name,
            bio,
        } => account_create(username, remote, display_name, bio),
    }
}

fn run_checks(action: ChecksAction, remote: &str) -> Result<(), String> {
    match action {
        ChecksAction::List => list::<Checks>(remote),
        ChecksAction::Add {
            name,
            command,
            image,
            depends,
        } => add_check(name, command, image, depends, remote),
        ChecksAction::Remove { name } => remove::<Checks>(&name, remote),
        ChecksAction::Debug => checks_debug(remote),
        ChecksAction::Runs => checks_runs(remote),
    }
}

/// Print every recorded check run on `remote`, newest commit first and
/// (within a commit) newest run first, as `<commit>  <when>  <check>=<status> …`.
fn checks_runs(remote: &str) -> Result<(), String> {
    let repo = repo()?;
    sync_namespace(remote, checks::RUNS_NS)?;
    let commits = checks::runs(&repo).map_err(|error| error.to_string())?;
    if commits.is_empty() {
        println!("no check runs on {remote}");
        return Ok(());
    }
    for commit_runs in commits {
        for run in &commit_runs.runs {
            let when = ago(run.at);
            let results = run
                .results
                .iter()
                .map(|outcome| format!("{}={}", outcome.name, outcome.status))
                .collect::<Vec<_>>()
                .join("  ");
            println!(
                "{}  {when}  {results}",
                short_id(&commit_runs.commit.to_string())
            );
        }
    }
    Ok(())
}

fn run_comment(action: CommentAction, remote: &str) -> Result<(), String> {
    match action {
        CommentAction::Add {
            path,
            body,
            lines,
            rev,
            issue,
        } => comment_add(path, body, lines.as_deref(), &rev, issue, remote),
        CommentAction::List { rev } => comment_list(remote, &rev),
        CommentAction::Show { id, rev } => comment_show(&id, remote, &rev),
        CommentAction::Remove { id } => comment_remove(&id, remote),
    }
}

/// Anchor a comment to `path` (and optionally `lines`) as it exists at `rev`,
/// record it at `refs/meta/comments/<id>` authored as the configured git
/// identity, and push it. Prompts for the path and body left `None` when run
/// at an interactive terminal.
fn comment_add(
    path: Option<String>,
    body: Option<String>,
    lines: Option<&str>,
    rev: &str,
    issue: Option<String>,
    remote: &str,
) -> Result<(), String> {
    let path = interactive::text_or(path, "File path")?;
    let body = interactive::text_or(body, "Comment")?;
    let lines = parse_lines(lines)?;
    let repo = repo()?;
    let anchor =
        git_anchor::capture(&repo, rev, &path, lines).map_err(|error| error.to_string())?;
    let comment = Comment {
        body,
        anchor,
        issue,
    };
    let id = git_comment::new_id(None, &comment).map_err(|error| error.to_string())?;
    let refname = format!("{COMMENTS_NS}/{id}");
    let expected = sync(remote, &refname)?;
    let name = config_get("user.name").ok_or("user.name is unset")?;
    let email = config_get("user.email").ok_or("user.email is unset")?;
    git_comment::store(&repo, &id, &comment, (&name, &email)).map_err(|error| error.to_string())?;
    push_signed(remote, &refname, expected.as_deref())?;
    println!("recorded comment {id}");
    Ok(())
}

/// List every comment on `remote` as `<id>  <author>  <location>  <body>`,
/// with each anchor projected onto `rev`.
fn comment_list(remote: &str, rev: &str) -> Result<(), String> {
    let repo = repo()?;
    sync_namespace(remote, COMMENTS_NS)?;
    let comments = git_comment::list(&repo).map_err(|error| error.to_string())?;
    if comments.is_empty() {
        println!("no comments on {remote}");
        return Ok(());
    }
    for (id, comment) in comments {
        let author = git_comment::provenance(&repo, &id)
            .map_err(|error| error.to_string())?
            .map_or_else(|| "?".to_owned(), |provenance| provenance.created.name);
        let place = describe_projection(&repo, &comment, rev);
        let title = comment.body.lines().next().unwrap_or_default();
        println!("{}  {author}  {place}  {title}", short_id(&id));
    }
    Ok(())
}

/// Show the comment `id` (or a unique prefix): who wrote and last edited it,
/// where it was anchored, where that sits on `rev`, the anchored text, and
/// the body.
fn comment_show(id: &str, remote: &str, rev: &str) -> Result<(), String> {
    let repo = repo()?;
    sync_namespace(remote, COMMENTS_NS)?;
    let id = resolve_comment_id(&repo, id, remote)?;
    let comment = git_comment::load(&repo, &id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("no comment {id} on {remote}"))?;
    println!("comment {id}");
    if let Some(provenance) =
        git_comment::provenance(&repo, &id).map_err(|error| error.to_string())?
    {
        println!(
            "author  {} <{}>",
            provenance.created.name, provenance.created.email
        );
        if provenance.updated != provenance.created {
            println!(
                "edited  {} <{}>",
                provenance.updated.name, provenance.updated.email
            );
        }
    }
    println!(
        "anchor  {} @ {}",
        location(&comment.anchor.path, comment.anchor.lines),
        short_id(&comment.anchor.commit.to_string())
    );
    println!("on {rev}: {}", describe_projection(&repo, &comment, rev));
    if let Some(issue) = &comment.issue {
        println!("issue   {issue}");
    }
    if comment.anchor.lines.is_some()
        && let Ok(snippet) = git_anchor::snippet(&repo, &comment.anchor)
    {
        println!();
        for line in snippet.lines() {
            println!("  | {line}");
        }
    }
    println!();
    for line in comment.body.lines() {
        println!("  {line}");
    }
    Ok(())
}

/// Remove the comment `id` (or a unique prefix) on `remote`, deleting its ref
/// and pushing the deletion.
fn comment_remove(id: &str, remote: &str) -> Result<(), String> {
    let repo = repo()?;
    sync_namespace(remote, COMMENTS_NS)?;
    let id = resolve_comment_id(&repo, id, remote)?;
    let refname = format!("{COMMENTS_NS}/{id}");
    let expected = sync(remote, &refname)?.ok_or_else(|| format!("no comment {id} on {remote}"))?;
    push_delete(remote, &refname, &expected)?;
    println!("removed comment {}", short_id(&id));
    Ok(())
}

/// Parse `--lines` as `<start>[:<end>]`, 1-based inclusive; a bare `<start>`
/// anchors that single line.
fn parse_lines(lines: Option<&str>) -> Result<Option<LineRange>, String> {
    let Some(lines) = lines else {
        return Ok(None);
    };
    let (start, end) = lines.split_once(':').unwrap_or((lines, lines));
    let parse = |number: &str| {
        number
            .trim()
            .parse::<u64>()
            .map_err(|_error| format!("invalid line number {number:?} in --lines"))
    };
    let start = parse(start)?;
    let end = parse(end)?;
    if start > end {
        return Err(format!(
            "--lines {start}:{end} is inverted (start must not come after end)"
        ));
    }
    Ok(Some(LineRange { start, end }))
}

/// Resolve `id` — a full comment genesis hash or a unique prefix of one —
/// against the synced local comment refs.
fn resolve_comment_id(repo: &Path, id: &str, remote: &str) -> Result<String, String> {
    let all = git_comment::list(repo).map_err(|error| error.to_string())?;
    let mut matches = all
        .into_iter()
        .map(|(full, _comment)| full)
        .filter(|full| full.starts_with(id));
    let Some(first) = matches.next() else {
        return Err(format!("no comment {id} on {remote}"));
    };
    if matches.next().is_some() {
        return Err(format!("comment id {id} is ambiguous on {remote}"));
    }
    Ok(first)
}

/// `path:lines` as the CLI prints an anchored location.
fn location(path: &str, lines: Option<LineRange>) -> String {
    match lines {
        Some(range) if range.start == range.end => format!("{path}:{}", range.start),
        Some(range) => format!("{path}:{}-{}", range.start, range.end),
        None => path.to_owned(),
    }
}

/// One-line description of where `comment` sits on `rev`.
fn describe_projection(repo: &Path, comment: &Comment, rev: &str) -> String {
    match git_comment::project(repo, comment, rev) {
        Ok(Projection::Current) => location(&comment.anchor.path, comment.anchor.lines),
        Ok(Projection::Relocated { path, lines }) => location(&path, lines),
        Ok(Projection::Outdated { path }) => format!("{path} [outdated]"),
        Ok(Projection::FileDeleted) => format!("{} [deleted]", comment.anchor.path),
        Err(_error) => format!("{} [unresolved]", comment.anchor.path),
    }
}

/// The first 12 characters of a hex id, as listings abbreviate it.
fn short_id(id: &str) -> &str {
    id.get(..12).unwrap_or(id)
}

/// A `refs/meta/*` set the porcelain manages uniformly: a named ref synced from
/// and pushed to a remote, holding entries the CLI lists and removes from. The
/// check set runs through this; the member set is decomposed across
/// `refs/meta/member/*` and handled on its own. A set differs only in its item
/// type, what the messages call an entry, and how a row presents; the thin
/// [`load`](Set::load)/[`store`](Set::store) keep each on its own typed module.
trait Set {
    /// The set's item type.
    type Item;
    /// The ref the set lives on.
    const REF: &'static str;
    /// The singular noun used in messages ("member", "check").
    const NOUN: &'static str;

    /// The set's items.
    fn load(repo: &Path) -> Result<Vec<Self::Item>, String>;
    /// Replace the set with `items`.
    fn store(repo: &Path, items: &[Self::Item]) -> Result<(), String>;
    /// The line printed when the set is empty on `remote`.
    fn empty_listing(remote: &str) -> String;
    /// An item's key — its identity for removal and the left list column.
    fn key(item: &Self::Item) -> String;
    /// The right list column for an item.
    fn value(item: &Self::Item) -> String;
}

/// The configured check set at `refs/meta/checks`.
struct Checks;

impl Set for Checks {
    type Item = Check;
    const REF: &'static str = CHECKS_REF;
    const NOUN: &'static str = "check";

    fn load(repo: &Path) -> Result<Vec<Check>, String> {
        checks::load(repo).map_err(|error| error.to_string())
    }

    fn store(repo: &Path, items: &[Check]) -> Result<(), String> {
        checks::store(repo, items).map_err(|error| error.to_string())
    }

    fn empty_listing(remote: &str) -> String {
        format!("no checks configured on {remote}")
    }

    fn key(item: &Check) -> String {
        item.name.clone()
    }

    fn value(item: &Check) -> String {
        let mut value = item
            .command
            .clone()
            .unwrap_or_else(|| "(composite)".to_owned());
        if let Some(image) = &item.image {
            value.push_str(&format!("  [image: {image}]"));
        }
        if !item.depends.is_empty() {
            value.push_str(&format!("  [needs: {}]", item.depends.join(", ")));
        }
        value
    }
}

/// The trailing ` (after …, before …)` annotation for a member's validity
/// window, or `""` when unbounded — so an expiry that has been set is visible at
/// a glance rather than hidden in the stored `allowed_signers` options.
fn window_suffix(member: &Member) -> String {
    let mut window = Vec::new();
    if let Some(after) = &member.valid_after {
        window.push(format!("after {after}"));
    }
    if let Some(before) = &member.valid_before {
        window.push(format!("before {before}"));
    }
    if window.is_empty() {
        String::new()
    } else {
        format!(" ({})", window.join(", "))
    }
}

/// Print each entry of the set `S` on `remote` as `<key>  <value>`.
fn list<S: Set>(remote: &str) -> Result<(), String> {
    let repo = repo()?;
    sync(remote, S::REF)?;
    let items = S::load(&repo)?;
    if items.is_empty() {
        println!("{}", S::empty_listing(remote));
        return Ok(());
    }
    for item in items {
        println!("{}  {}", S::key(&item), S::value(&item));
    }
    Ok(())
}

/// Drop the entry keyed `key` from the set `S` on `remote` and push the update.
fn remove<S: Set>(key: &str, remote: &str) -> Result<(), String> {
    let repo = repo()?;
    let expected = sync(remote, S::REF)?;
    let before = S::load(&repo)?;
    let count = before.len();
    let after: Vec<S::Item> = before
        .into_iter()
        .filter(|item| S::key(item) != key)
        .collect();
    if after.len() == count {
        return Err(format!("no {} named {key} on {remote}", S::NOUN));
    }
    S::store(&repo, &after)?;
    push_signed(remote, S::REF, expected.as_deref())?;
    println!("removed {key}");
    Ok(())
}

/// Add `name` running `command` to `remote`'s set, replacing any check already
/// recorded under that name, and push the update. Prompts for any field left
/// unset when run at an interactive terminal. The whole set is validated as a
/// dependency graph (`checks::order`) before it is stored, so a cycle or a
/// dangling dependency never lands on the remote.
fn add_check(
    name: Option<String>,
    command: Option<String>,
    image: Option<String>,
    depends: Vec<String>,
    remote: &str,
) -> Result<(), String> {
    let name = interactive::text_or(name, "Check name")?;
    let command = interactive::optional_text_or(command, "Command (empty for a composite)")?;
    let depends = if depends.is_empty() {
        parse_depends(interactive::optional_text_or(
            None,
            "Depends on (comma-separated, empty for none)",
        )?)
    } else {
        depends
    };
    let repo = repo()?;
    let expected = sync(remote, CHECKS_REF)?;
    let mut checks = checks::load(&repo).map_err(|error| error.to_string())?;
    checks.retain(|check| check.name != name);
    checks.push(Check {
        name: name.clone(),
        command,
        image,
        depends,
    });
    let _ordered = checks::order(&checks)?;
    checks::store(&repo, &checks).map_err(|error| error.to_string())?;
    push_signed(remote, CHECKS_REF, expected.as_deref())?;
    println!("recorded check {name}");
    Ok(())
}

/// Split an interactive comma-separated dependency reply into names, dropping
/// empty segments; `None` (no reply) is no dependencies.
fn parse_depends(reply: Option<String>) -> Vec<String> {
    reply
        .map(|value| {
            value
                .split(',')
                .map(|name| name.trim().to_owned())
                .filter(|name| !name.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Set this machine up to produce the signed pushes the server requires:
/// ensure a signing key exists, then record the SSH signing config
/// (SSH-format signatures, the key, and "sign when the server asks" so pushes
/// elsewhere are untouched). Writes global config by default, since the setup
/// is per-machine.
fn setup(key: Option<&Path>, local: bool) -> Result<(), String> {
    let scope = if local { "--local" } else { "--global" };
    let signing_key = match key {
        Some(path) => ensure_key(path)?,
        None => match config_get("user.signingkey") {
            Some(existing) => ensure_key(&signing_key_path(&existing))?,
            None => ensure_key(&default_key_path()?)?,
        },
    };
    set_config(scope, "gpg.format", "ssh")?;
    set_config(scope, "user.signingkey", &signing_key)?;
    set_config(scope, "push.gpgSign", "if-asked")?;

    let public_key = public_key(None)?;
    let fingerprint = fingerprint(&public_key)?;
    println!(
        "configured signed pushes ({} git config)",
        scope.trim_start_matches('-')
    );
    println!("signing key: {signing_key} ({fingerprint})");
    println!("authorize it on a server with `git ents members add <remote>`");
    Ok(())
}

/// Ensure a usable SSH key exists at `path`, returning the public-key path to
/// record in `user.signingkey`. Generates an ed25519 keypair when neither the
/// key nor its `.pub` is present; derives a missing `.pub` from the private key.
fn ensure_key(path: &Path) -> Result<String, String> {
    let (private, public) = key_paths(path);
    if public.exists() {
        return Ok(public.display().to_string());
    }
    if private.exists() {
        let derived = read_public_key(&private)?;
        std::fs::write(&public, format!("{derived}\n"))
            .map_err(|error| format!("could not write {}: {error}", public.display()))?;
        return Ok(public.display().to_string());
    }
    if !confirm(&format!(
        "no SSH key at {}; generate a new ed25519 keypair there?",
        private.display()
    ))? {
        return Err("setup needs a signing key; re-run with `--key` or generate one".to_owned());
    }
    generate_key(&private)?;
    Ok(public.display().to_string())
}

/// Resolve the path to ensure for a configured `user.signingkey`. A real key
/// path (or one a `.pub` can be derived from) is used as-is; a bare key id —
/// e.g. an openpgp fingerprint left from another signing format — is not a
/// path, so fall back to the default SSH key location rather than generating a
/// keypair named after it.
fn signing_key_path(configured: &str) -> PathBuf {
    let candidate = expand_tilde(configured);
    let (private, public) = key_paths(&candidate);
    if private.exists() || public.exists() || configured.contains('/') {
        candidate
    } else {
        default_key_path().unwrap_or(candidate)
    }
}

/// Ask `question` on the terminal, returning whether it was accepted. Enter
/// (an empty reply) accepts; a reply starting with `n` declines.
fn confirm(question: &str) -> Result<bool, String> {
    use std::io::Write as _;
    print!("{question} [Y/n] ");
    std::io::stdout()
        .flush()
        .map_err(|error| format!("could not write prompt: {error}"))?;
    let mut reply = String::new();
    std::io::stdin()
        .read_line(&mut reply)
        .map_err(|error| format!("could not read reply: {error}"))?;
    let reply = reply.trim();
    Ok(reply.is_empty() || !reply.starts_with(['n', 'N']))
}

/// Split a key path into its private and `.pub` halves.
fn key_paths(path: &Path) -> (PathBuf, PathBuf) {
    if path.extension().is_some_and(|extension| extension == "pub") {
        (path.with_extension(""), path.to_owned())
    } else {
        (
            path.to_owned(),
            PathBuf::from(format!("{}.pub", path.display())),
        )
    }
}

/// Generate a passphrase-less ed25519 keypair at `private` and `<private>.pub`.
fn generate_key(private: &Path) -> Result<(), String> {
    if let Some(dir) = private.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir)
            .map_err(|error| format!("could not create {}: {error}", dir.display()))?;
    }
    println!("generating a new ed25519 key at {}", private.display());
    let status = Command::new("ssh-keygen")
        .arg("-t")
        .arg("ed25519")
        .arg("-N")
        .arg("")
        .arg("-C")
        .arg(host_comment())
        .arg("-f")
        .arg(private)
        .status()
        .map_err(|error| format!("could not run ssh-keygen: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("ssh-keygen could not generate a key".to_owned())
    }
}

/// A `<user>@<host>` comment for a freshly generated key, best-effort.
fn host_comment() -> String {
    let user = std::env::var("USER").unwrap_or_else(|_unset| "git-ents".to_owned());
    match Command::new("hostname").output() {
        Ok(output) if output.status.success() => {
            let host = String::from_utf8_lossy(&output.stdout);
            let host = host.trim();
            if host.is_empty() {
                user
            } else {
                format!("{user}@{host}")
            }
        }
        Ok(_) | Err(_) => user,
    }
}

/// The default signing key path, `~/.ssh/id_ed25519`.
fn default_key_path() -> Result<PathBuf, String> {
    let home = std::env::var("HOME").map_err(|_unset| "HOME is not set".to_owned())?;
    Ok(Path::new(&home).join(".ssh").join("id_ed25519"))
}

/// List every member on `remote` — one line per authorized key, or one
/// `cert-authority` line per pinned-CA member — as
/// `<username>[/<fingerprint>]  <label><window>`, flagging keys on the
/// `refs/meta/revoked` deny list as `[revoked]`.
fn members_list(remote: &str) -> Result<(), String> {
    let repo = repo()?;
    sync_namespace(remote, MEMBER_NS)?;
    sync(remote, REVOKED_REF)?;
    let members = members::load_all(&repo).map_err(|error| error.to_string())?;
    if members.is_empty() {
        println!("no members on {remote} (open bootstrap window)");
        return Ok(());
    }
    let revoked = revocations::fingerprints(&repo).map_err(|error| error.to_string())?;
    for member in members {
        let suffix = window_suffix(&member);
        if let Some(ca) = member.ca() {
            println!(
                "{}  cert-authority {}{suffix}",
                member.principal,
                key_comment(ca)
            );
        } else {
            for (fingerprint, key) in member.keys() {
                let flag = if revoked.contains(fingerprint) {
                    " [revoked]"
                } else {
                    ""
                };
                println!(
                    "{}/{fingerprint}  {}{suffix}{flag}",
                    member.principal,
                    key_comment(key)
                );
            }
        }
    }
    Ok(())
}

/// Add `fingerprint` to `remote`'s `refs/meta/revoked` deny list and push the
/// update, so the key is refused before its window would expire.
fn members_revoke(fingerprint: &str, remote: &str, reason: String) -> Result<(), String> {
    if !looks_like_fingerprint(fingerprint) {
        return Err(format!(
            "{fingerprint:?} does not look like a key fingerprint \
             (expected colon-hex form, e.g. aa:bb:cc:..., as `members list` prints)"
        ));
    }
    let repo = repo()?;
    // Revoking your own key fails closed against you too: if it is the last key
    // that authorizes your pushes, you cannot even push the un-revoke. Warn
    // before locking yourself out.
    if own_fingerprint().is_some_and(|own| own == fingerprint)
        && !confirm(&format!(
            "{fingerprint} is your own signing key; \
             revoking it may lock you out of {remote}. Continue?"
        ))?
    {
        return Err("revocation cancelled".to_owned());
    }
    let expected = sync(remote, REVOKED_REF)?;
    let mut revocations = revocations::load(&repo).map_err(|error| error.to_string())?;
    if let Some(existing) = revocations
        .iter_mut()
        .find(|revocation| revocation.fingerprint == fingerprint)
    {
        existing.reason = reason;
    } else {
        revocations.push(Revocation {
            fingerprint: fingerprint.to_owned(),
            reason,
        });
    }
    revocations::store(&repo, &revocations).map_err(|error| error.to_string())?;
    push_signed(remote, REVOKED_REF, expected.as_deref())?;
    println!("revoked {fingerprint}");
    Ok(())
}

/// Remove `fingerprint` from `remote`'s `refs/meta/revoked` deny list and push
/// the update.
fn members_unrevoke(fingerprint: &str, remote: &str) -> Result<(), String> {
    let repo = repo()?;
    let expected = sync(remote, REVOKED_REF)?;
    let mut revocations = revocations::load(&repo).map_err(|error| error.to_string())?;
    let before = revocations.len();
    revocations.retain(|revocation| revocation.fingerprint != fingerprint);
    if revocations.len() == before {
        return Err(format!("{fingerprint} is not revoked on {remote}"));
    }
    revocations::store(&repo, &revocations).map_err(|error| error.to_string())?;
    push_signed(remote, REVOKED_REF, expected.as_deref())?;
    println!("lifted revocation of {fingerprint}");
    Ok(())
}

/// The `key`/`cert_authority` pair for [`members_add`]. Used as given when
/// either is already set or the terminal is non-interactive, so
/// `--key`/`--cert-authority` and scripted runs are unchanged; otherwise
/// prompts for which kind of trust to add.
fn resolve_trust(
    key: Option<PathBuf>,
    cert_authority: Option<PathBuf>,
) -> Result<(Option<PathBuf>, Option<PathBuf>), String> {
    if key.is_some() && cert_authority.is_some() {
        return Err("--key conflicts with --cert-authority".to_string());
    }
    if key.is_some() || cert_authority.is_some() || !interactive::available() {
        return Ok((key, cert_authority));
    }
    let choice = interactive::select_or("Trust", &["Signing key", "Certificate authority"], 0)?;
    if choice == 1 {
        let path = interactive::text_or(None, "Certificate authority public key path")?;
        Ok((None, Some(PathBuf::from(path))))
    } else {
        let path =
            interactive::optional_text_or(None, "Signing key path (blank for user.signingkey)")?;
        Ok((path.map(PathBuf::from), None))
    }
}

/// Authorize a key (or pin a CA) for the member `username` on `remote`, trusting
/// the member within the given validity window, and push the updated member ref.
fn members_add(
    username: Option<String>,
    remote: &str,
    key: Option<PathBuf>,
    cert_authority: Option<PathBuf>,
    valid_after: Option<String>,
    valid_before: Option<String>,
    account: Option<String>,
) -> Result<(), String> {
    let username = interactive::text_or(username, "Username")?;
    let (key, cert_authority) = resolve_trust(key, cert_authority)?;
    let valid_after = interactive::optional_text_or(valid_after, "Valid after (blank for none)")?;
    let valid_before =
        interactive::optional_text_or(valid_before, "Valid before (blank for none)")?;
    let account =
        interactive::optional_text_or(account, "Link to account (genesis hash, blank to skip)")?;
    if let Some(after) = &valid_after {
        validate_timestamp(after)?;
    }
    if let Some(before) = &valid_before {
        validate_timestamp(before)?;
    }
    let repo = repo()?;
    let refname = member_ref(&username);
    let expected = sync(remote, &refname)?;
    let mut member = members::load(&repo, &username)
        .map_err(|error| error.to_string())?
        .unwrap_or_else(|| Member::with_keys(username.clone(), BTreeMap::new()));
    if valid_after.is_some() {
        member.valid_after = valid_after;
    }
    if valid_before.is_some() {
        member.valid_before = valid_before;
    }
    if account.is_some() {
        member.account = account;
    }

    // Pinning a CA replaces the member's trust wholesale — a member is either
    // leaf keys or a CA, never both.
    if let Some(ca_path) = cert_authority {
        let ca = read_public_key(&ca_path)?;
        member.trust = Trust::CertAuthority(ca);
        members::store(&repo, &member).map_err(|error| error.to_string())?;
        push_signed(remote, &refname, expected.as_deref())?;
        println!("pinned a certificate authority for {username}");
        return Ok(());
    }

    let public_key = public_key(key.as_deref())?;
    let fingerprint = fingerprint(&public_key)?;
    let keys = match &mut member.trust {
        Trust::Keys(keys) => keys,
        Trust::CertAuthority(_ca) => {
            return Err(format!(
                "{username} is pinned to a certificate authority; \
                 revoke and re-add to switch to leaf keys"
            ));
        }
        Trust::WebAuthn(_keys) => {
            return Err(format!(
                "{username} is a self-attested WebAuthn member; \
                 an admin must promote them before adding leaf keys"
            ));
        }
    };
    if keys
        .values()
        .any(|existing| same_key(existing, &public_key))
    {
        println!("{fingerprint} is already authorized for {username}");
        return Ok(());
    }
    keys.insert(fingerprint.clone(), public_key);
    members::store(&repo, &member).map_err(|error| error.to_string())?;
    push_signed(remote, &refname, expected.as_deref())?;
    println!("authorized {fingerprint} for {username}");
    Ok(())
}

/// Revoke the member `username` on `remote`, deleting its ref and pushing the
/// deletion. Removal here is a plain signed delete; quorum-gated removal is a
/// later server-side policy.
fn members_remove(username: &str, remote: &str) -> Result<(), String> {
    let refname = member_ref(username);
    let expected =
        sync(remote, &refname)?.ok_or_else(|| format!("no member named {username} on {remote}"))?;
    push_delete(remote, &refname, &expected)?;
    println!("revoked {username}");
    Ok(())
}

/// Create or update this repository's account identity on `remote` and push it.
fn account_create(
    username: Option<String>,
    remote: &str,
    display_name: Option<String>,
    bio: Option<String>,
) -> Result<(), String> {
    let username = interactive::text_or(username, "Username")?;
    let display_name =
        interactive::optional_text_or(display_name, "Display name (blank to use username)")?;
    let bio = interactive::optional_text_or(bio, "Bio (blank to skip)")?.unwrap_or_default();
    let repo = repo()?;
    let expected = sync(remote, account::ACCOUNT_REF)?;
    let existing = account::load(&repo).map_err(|error| error.to_string())?;
    let is_update = existing.is_some();
    let account = Account {
        username: username.clone(),
        display_name: display_name.unwrap_or_else(|| username.clone()),
        bio,
        // Preserve the original creation time when updating an existing account.
        created_at: existing.map_or_else(now_seconds, |account| account.created_at),
    };
    account::store(&repo, &account).map_err(|error| error.to_string())?;
    push_signed(remote, account::ACCOUNT_REF, expected.as_deref())?;
    let genesis = account::genesis(&repo).map_err(|error| error.to_string())?;
    if is_update {
        println!("updated account {username}");
    } else {
        println!("created account {username}");
    }
    if let Some(genesis) = genesis {
        println!("genesis: {genesis} (pass to `members add --account` to link a member)");
    }
    Ok(())
}

/// The SSHSIG namespace a sign-in signature is made under; must match the
/// server's `git-ents-server::web::write::LOGIN_NAMESPACE`.
const LOGIN_NAMESPACE: &str = "git.ents.cloud";

/// Sign in to `remote`'s server: fetch its one-time challenge, sign it locally
/// with `key` (never handing the private key anywhere), and post the
/// signature back — the same proof the browser login page collects by hand.
/// The returned session token is stored locally so `checks_debug` can reuse
/// it.
fn login(remote: &str, key: Option<&Path>) -> Result<(), String> {
    let (base, _repo_path) = remote_http_base(remote)?;
    let private_key = signing_key_file(key)?;
    let public_key = public_key(key)?;

    let nonce = http_get(&format!("{base}/login/cli"))?;
    let signature = sign_challenge(&private_key, &nonce)?;
    let body = form_urlencoded::Serializer::new(String::new())
        .append_pair("public_key", &public_key)
        .append_pair("signature", &signature)
        .append_pair("nonce", &nonce)
        .finish();
    let token = http_post_form(&format!("{base}/login/cli"), &body)?;

    store_session(&host_of(&base)?, &token)?;
    println!("signed in to {remote}");
    Ok(())
}

/// Open an interactive, read-write shell in `remote`'s persistent checks
/// Sprite, brokered by the server over a WebSocket using the session
/// `login` stored.
fn checks_debug(remote: &str) -> Result<(), String> {
    let (base, repo_path) = remote_http_base(remote)?;
    let host = host_of(&base)?;
    let token = load_session(&host)?
        .ok_or_else(|| format!("not signed in to {remote}; run `git ents login {remote}` first"))?;
    let ws_url = format!("{}/_debug/{repo_path}", to_ws(&base));

    let runtime = tokio::runtime::Runtime::new()
        .map_err(|error| format!("could not start the async runtime: {error}"))?;
    runtime.block_on(crate::debug_session::run(&ws_url, &token))
}

/// The path to the private half of the signing key to use: `key` verbatim, or
/// the path behind `user.signingkey`, resolved the same way `setup` does.
fn signing_key_file(key: Option<&Path>) -> Result<PathBuf, String> {
    match key {
        Some(path) => Ok(key_paths(path).0),
        None => {
            let configured = config_get("user.signingkey")
                .ok_or("no --key given and user.signingkey is unset")?;
            Ok(key_paths(&signing_key_path(&configured)).0)
        }
    }
}

/// Sign `nonce` under [`LOGIN_NAMESPACE`] with the private key at `path`,
/// returning the armored SSH signature. `ssh-keygen -Y sign` only writes a
/// signature next to a file it read, so the nonce is staged there first.
fn sign_challenge(private_key: &Path, nonce: &str) -> Result<String, String> {
    let dir = tempfile::tempdir().map_err(|error| format!("could not create temp dir: {error}"))?;
    let data = dir.path().join("nonce");
    std::fs::write(&data, nonce).map_err(|error| format!("could not write challenge: {error}"))?;
    let status = Command::new("ssh-keygen")
        .args(["-Y", "sign", "-f"])
        .arg(private_key)
        .args(["-n", LOGIN_NAMESPACE])
        .arg(&data)
        .status()
        .map_err(|error| format!("could not run ssh-keygen: {error}"))?;
    if !status.success() {
        return Err("ssh-keygen could not sign the challenge".to_owned());
    }
    std::fs::read_to_string(dir.path().join("nonce.sig"))
        .map_err(|error| format!("could not read the signature: {error}"))
}

/// The server's http(s) base URL and repository path (without `.git`) for
/// `remote`'s configured URL, e.g. `https://ents.example.com` and `org/repo`.
fn remote_http_base(remote: &str) -> Result<(String, String), String> {
    let url = git_capture(&["remote", "get-url", remote])?;
    let url = url.trim();
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| format!("{remote} is not an http(s) remote; login and debug need one"))?;
    if scheme != "http" && scheme != "https" {
        return Err(format!(
            "{remote} is not an http(s) remote; login and debug need one"
        ));
    }
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let repo_path = path.strip_suffix(".git").unwrap_or(path).trim_matches('/');
    Ok((format!("{scheme}://{host}"), repo_path.to_owned()))
}

/// The `host[:port]` portion of an `http(s)://host[:port]` base URL.
fn host_of(base: &str) -> Result<String, String> {
    base.split_once("://")
        .map(|(_scheme, host)| host.to_owned())
        .ok_or_else(|| "malformed server URL".to_owned())
}

/// Rewrite an `http(s)://` base URL to its `ws(s)://` equivalent.
fn to_ws(base: &str) -> String {
    if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        base.to_owned()
    }
}

/// GET `url`, returning the response body, or its body text as the error on a
/// non-2xx status.
fn http_get(url: &str) -> Result<String, String> {
    let mut response = ureq::get(url)
        .config()
        .http_status_as_error(false)
        .build()
        .call()
        .map_err(|error| format!("GET {url} failed: {error}"))?;
    let status = response.status();
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|error| format!("could not read the response: {error}"))?;
    if status.is_success() {
        Ok(text)
    } else if text.is_empty() {
        Err(format!("GET {url} returned {status}"))
    } else {
        Err(text)
    }
}

/// POST an `application/x-www-form-urlencoded` `body` to `url`, returning the
/// response body, or its body text as the error on a non-2xx status.
fn http_post_form(url: &str, body: &str) -> Result<String, String> {
    let mut response = ureq::post(url)
        .config()
        .http_status_as_error(false)
        .build()
        .header("Content-Type", "application/x-www-form-urlencoded")
        .send(body)
        .map_err(|error| format!("POST {url} failed: {error}"))?;
    let status = response.status();
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|error| format!("could not read the response: {error}"))?;
    if status.is_success() {
        Ok(text)
    } else if text.is_empty() {
        Err(format!("POST {url} returned {status}"))
    } else {
        Err(text)
    }
}

/// Where `login` stores the session token for `host`, one file per host.
fn session_path(host: &str) -> Result<PathBuf, String> {
    let home = std::env::var("HOME").map_err(|_unset| "HOME is not set".to_owned())?;
    let sanitized: String = host
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    Ok(Path::new(&home)
        .join(".config/git-ents/sessions")
        .join(sanitized))
}

/// Persist the session `token` for `host`, restricted to the owner.
fn store_session(host: &str, token: &str) -> Result<(), String> {
    let path = session_path(host)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|error| format!("could not create {}: {error}", dir.display()))?;
    }
    std::fs::write(&path, token).map_err(|error| format!("could not write session: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _permissions = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// The stored session token for `host`, if `login` has been run against it.
fn load_session(host: &str) -> Result<Option<String>, String> {
    match std::fs::read_to_string(session_path(host)?) {
        Ok(token) => Ok(Some(token.trim().to_owned())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("could not read the stored session: {error}")),
    }
}

/// This client's own signing-key fingerprint, best-effort — `None` when no key
/// is configured or it cannot be read.
fn own_fingerprint() -> Option<String> {
    let public_key = public_key(None).ok()?;
    fingerprint(&public_key).ok()
}

/// The current time as seconds since the Unix epoch.
fn now_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// `at` (seconds since the Unix epoch) as a relative "N units ago" string.
fn ago(at: u64) -> String {
    let secs = now_seconds().saturating_sub(at);
    let mins = secs / 60;
    let hours = mins / 60;
    let days = hours / 24;
    if mins == 0 {
        "just now".to_owned()
    } else if hours == 0 {
        format!("{mins}m ago")
    } else if days == 0 {
        format!("{hours}h ago")
    } else {
        format!("{days}d ago")
    }
}

/// Fail-fast check, ahead of any network sync, that `value` is a well-formed
/// OpenSSH `allowed_signers` timestamp — the same rule [`Member::validate`]
/// (via [`members::store`]) checks again before the write actually lands, and
/// which also checks the two bounds are not inverted.
fn validate_timestamp(value: &str) -> Result<(), String> {
    if members::valid_timestamp(value) {
        Ok(())
    } else {
        Err(format!(
            "invalid timestamp {value:?}: expected YYYYMMDD[Z] or YYYYMMDDHHMM[SS][Z]"
        ))
    }
}

/// Report whether `key` is a member on `remote` and how this client is
/// configured.
fn check(remote: &str, key: Option<&Path>) -> Result<(), String> {
    let repo = repo()?;
    let public_key = public_key(key)?;
    let fingerprint = fingerprint(&public_key)?;
    sync_namespace(remote, MEMBER_NS)?;
    let members = members::load_all(&repo).map_err(|error| error.to_string())?;
    if members.is_empty() {
        println!("{remote}: open bootstrap window (no members yet)");
    } else if let Some(member) = members.iter().find(|member| {
        member
            .keys()
            .iter()
            .any(|(_fp, k)| same_key(k, &public_key))
    }) {
        println!("{remote}: {fingerprint} is a member ({})", member.principal);
    } else {
        println!("{remote}: {fingerprint} is NOT a member");
    }
    println!(
        "client: gpg.format={}, user.signingkey={}, push.gpgSign={}",
        config_get("gpg.format").as_deref().unwrap_or("(unset)"),
        config_get("user.signingkey")
            .as_deref()
            .unwrap_or("(unset)"),
        config_get("push.gpgSign").as_deref().unwrap_or("(unset)"),
    );
    Ok(())
}

/// The repository to operate in: the current working directory's clone.
fn repo() -> Result<PathBuf, String> {
    std::env::current_dir().map_err(|error| format!("cannot resolve current directory: {error}"))
}

/// Mirror `remote`'s `refname` into the local repository so the set helpers see
/// the current value, returning the remote's current object id (or `None` when
/// it has no such ref — for the signer set, the open bootstrap window). When the
/// remote has none, clear any stale local ref so the set reads empty.
fn sync(remote: &str, refname: &str) -> Result<Option<String>, String> {
    let listing = ls_remote(remote, refname)?;
    let oid = listing.split_whitespace().next().map(str::to_owned);
    if oid.is_some() {
        let refspec = format!("+{refname}:{refname}");
        git_run(&["fetch", "--quiet", remote, &refspec])?;
    } else {
        let _deleted = git_capture(&["update-ref", "-d", refname]);
    }
    Ok(oid)
}

/// Mirror every ref under `remote`'s `namespace` (e.g. `refs/meta/member`) into
/// the local repository, pruning local refs the remote no longer has, so the
/// glob helpers see the remote's current set.
fn sync_namespace(remote: &str, namespace: &str) -> Result<(), String> {
    let refspec = format!("+{namespace}/*:{namespace}/*");
    git_run(&["fetch", "--quiet", "--prune", remote, &refspec])
}

/// Push the local `refname` to `remote`, signed per the client's config.
///
/// `expected` is the remote tip observed at sync time (`None` when the ref did
/// not exist). Pushing with `--force-with-lease` pinned to that value, plus
/// `--force-if-includes`, makes the update a clean compare-and-swap: it is
/// rejected rather than clobbering a set someone changed since the fetch.
fn push_signed(remote: &str, refname: &str, expected: Option<&str>) -> Result<(), String> {
    let lease = format!(
        "--force-with-lease={refname}:{}",
        expected.unwrap_or(git_ents::ZERO_OID)
    );
    git_run(&["push", "--force-if-includes", &lease, remote, refname])
}

/// Delete `refname` on `remote`, signed per the client's config and pinned with
/// `--force-with-lease` to the `expected` tip so a member changed since the
/// fetch is not clobbered.
fn push_delete(remote: &str, refname: &str, expected: &str) -> Result<(), String> {
    let lease = format!("--force-with-lease={refname}:{expected}");
    let refspec = format!(":{refname}");
    git_run(&["push", "--force-if-includes", &lease, remote, &refspec])
}

/// Resolve the OpenSSH public key to operate on, defaulting to the key behind
/// `user.signingkey`.
fn public_key(key: Option<&Path>) -> Result<String, String> {
    match key {
        Some(path) => read_public_key(path),
        None => {
            let configured = config_get("user.signingkey")
                .ok_or("no --key given and user.signingkey is unset")?;
            if let Some(inline) = configured.strip_prefix("key::") {
                return Ok(inline.trim().to_owned());
            }
            read_public_key(&expand_tilde(&configured))
        }
    }
}

/// Read an OpenSSH public key from `path`, accepting either a `.pub` file or a
/// private key (whose public half is derived with `ssh-keygen -y`).
fn read_public_key(path: &Path) -> Result<String, String> {
    if let Ok(contents) = std::fs::read_to_string(path)
        && looks_like_public_key(&contents)
    {
        return Ok(contents.trim().to_owned());
    }
    let dotpub = PathBuf::from(format!("{}.pub", path.display()));
    if let Ok(contents) = std::fs::read_to_string(&dotpub)
        && looks_like_public_key(&contents)
    {
        return Ok(contents.trim().to_owned());
    }
    let output = Command::new("ssh-keygen")
        .arg("-y")
        .arg("-f")
        .arg(path)
        .output()
        .map_err(|error| format!("could not run ssh-keygen: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "could not read a public key from {}",
            path.display()
        ));
    }
    String::from_utf8(output.stdout)
        .map(|key| key.trim().to_owned())
        .map_err(|_invalid| "ssh-keygen produced non-UTF-8 output".to_owned())
}

/// Whether `text` opens with an OpenSSH public key type token.
fn looks_like_public_key(text: &str) -> bool {
    let head = text.trim_start();
    head.starts_with("ssh-") || head.starts_with("ecdsa-") || head.starts_with("sk-")
}

/// Expand a leading `~/` against `$HOME`.
fn expand_tilde(value: &str) -> PathBuf {
    if let Some(rest) = value.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return Path::new(&home).join(rest);
    }
    PathBuf::from(value)
}

/// The key's MD5 fingerprint in colon form (`aa:bb:…`). Colon-separated pairs
/// are filesystem-safe, unlike the slashes in a base64 SHA256 fingerprint that
/// would split the `members/<name>` entry into a subtree.
fn fingerprint(public_key: &str) -> Result<String, String> {
    let scratch =
        tempfile::tempdir().map_err(|error| format!("could not create temp dir: {error}"))?;
    let path = scratch.path().join("key.pub");
    std::fs::write(&path, public_key).map_err(|error| format!("could not stage key: {error}"))?;
    let output = Command::new("ssh-keygen")
        .arg("-E")
        .arg("md5")
        .arg("-l")
        .arg("-f")
        .arg(&path)
        .output()
        .map_err(|error| format!("could not run ssh-keygen: {error}"))?;
    if !output.status.success() {
        return Err("ssh-keygen could not fingerprint the key".to_owned());
    }
    let text = String::from_utf8(output.stdout)
        .map_err(|_invalid| "ssh-keygen produced non-UTF-8 output".to_owned())?;
    let field = text
        .split_whitespace()
        .nth(1)
        .ok_or("ssh-keygen returned an unexpected fingerprint line")?;
    Ok(field.strip_prefix("MD5:").unwrap_or(field).to_owned())
}

/// Whether `text` looks like an MD5 key fingerprint (`aa:bb:...`): only
/// colon-separated two-digit hex groups, matching what `members list` prints
/// and [`fingerprint`] produces.
fn looks_like_fingerprint(text: &str) -> bool {
    let groups: Vec<&str> = text.split(':').collect();
    groups.len() > 1
        && groups
            .iter()
            .all(|group| group.len() == 2 && group.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Whether two OpenSSH public keys share a type and body, ignoring the comment.
fn same_key(a: &str, b: &str) -> bool {
    key_body(a) == key_body(b)
}

/// A key's `(type, base64-body)`, the part that identifies it.
fn key_body(key: &str) -> (Option<&str>, Option<&str>) {
    let mut fields = key.split_whitespace();
    (fields.next(), fields.next())
}

/// A key's trailing comment, or an empty string when it has none.
fn key_comment(key: &str) -> String {
    let mut fields = key.split_whitespace();
    let _type = fields.next();
    let _body = fields.next();
    fields.collect::<Vec<_>>().join(" ")
}

/// Read a git config value, treating absent or empty as unset.
fn config_get(key: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["config", "--get", key])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_owned())
    }
}

/// Set a git config key, failing if git does.
fn set_config(scope: &str, key: &str, value: &str) -> Result<(), String> {
    git_run(&["config", scope, key, value])
}

/// Run git with inherited stdio, erroring on a non-zero exit.
fn git_run(args: &[&str]) -> Result<(), String> {
    let status = Command::new("git")
        .args(args)
        .status()
        .map_err(|error| format!("failed to run git: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "git {} failed",
            args.first().copied().unwrap_or("?")
        ))
    }
}

/// List `refname` on `remote`, translating git's raw "not found" fatal output
/// into a single message in the CLI's own words rather than stacking git's
/// `fatal: ...` lines above it; other failures fall back to git's own
/// (trimmed) stderr.
fn ls_remote(remote: &str, refname: &str) -> Result<String, String> {
    let output = Command::new("git")
        .args(["ls-remote", remote, refname])
        .output()
        .map_err(|error| format!("failed to run git: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("does not appear to be a git repository")
            || stderr.contains("Could not read from remote repository")
        {
            return Err(format!("remote '{remote}' not found"));
        }
        return Err(stderr.trim().to_owned());
    }
    String::from_utf8(output.stdout).map_err(|_invalid| "git produced non-UTF-8 output".to_owned())
}

/// Run git and capture its stdout (stderr inherited), erroring on a non-zero
/// exit.
fn git_capture(args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .stderr(Stdio::inherit())
        .output()
        .map_err(|error| format!("failed to run git: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed",
            args.first().copied().unwrap_or("?")
        ));
    }
    String::from_utf8(output.stdout).map_err(|_invalid| "git produced non-UTF-8 output".to_owned())
}
