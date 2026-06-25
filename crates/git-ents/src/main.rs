//! `git ents` — the git-ents command-line porcelain.
//!
//! It carries `git ents members` for managing the repository members recorded
//! one-ref-per-person at `refs/meta/member/<username>`, `git ents account` for
//! the account identity at `refs/meta/account`, `git ents checks` for the check
//! set, and the client setup that produces the signed pushes the server
//! requires. The member commands read and write a remote's set by fetching the
//! `refs/meta/member/*` refs into the local repository, editing them through
//! [`git_ents::signers`], and pushing them back.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use clap::{Parser, Subcommand};
use git_ents::account::{self, Account};
use git_ents::checks::{self, CHECKS_REF, Check};
use git_ents::revocations::{self, REVOKED_REF, Revocation};
use git_ents::signers::{self, MEMBER_NS, Member, Trust, member_ref};

#[derive(Parser)]
#[command(name = "git-ents", about = "Helpful guardians of your git trees.")]
struct Cli {
    #[command(subcommand)]
    command: Top,
}

#[derive(Subcommand)]
enum Top {
    /// Manage the repository members at `refs/meta/member/<username>`.
    Members {
        #[command(subcommand)]
        action: Action,
    },
    /// Manage this repository's account identity at `refs/meta/account`.
    Account {
        #[command(subcommand)]
        action: AccountAction,
    },
    /// Manage the configured checks at `refs/meta/checks`.
    Checks {
        #[command(subcommand)]
        action: ChecksAction,
    },
}

#[derive(Subcommand)]
enum Action {
    /// Set this machine up to sign the pushes the server requires.
    Setup {
        /// Key to sign with; defaults to `user.signingkey`, else a new or
        /// existing `~/.ssh/id_ed25519`.
        #[arg(long)]
        key: Option<PathBuf>,
        /// Write to this repository's config instead of your global config.
        #[arg(long)]
        local: bool,
    },
    /// List the members on a remote.
    List {
        /// Remote to read the `refs/meta/member/*` refs from.
        #[arg(default_value = "origin")]
        remote: String,
    },
    /// Authorize a key for a member on a remote and push the update.
    Add {
        /// Member (username) to authorize the key under — its
        /// `refs/meta/member/<username>` ref.
        username: String,
        /// Remote whose member refs to update.
        #[arg(default_value = "origin")]
        remote: String,
        /// Key to authorize; defaults to `user.signingkey`.
        #[arg(long)]
        key: Option<PathBuf>,
        /// Pin a certificate authority public key instead of leaf keys: trust
        /// any certificate it issues for the member, within the cert's validity.
        #[arg(long, value_name = "CA_PUBKEY", conflicts_with = "key")]
        cert_authority: Option<PathBuf>,
        /// Trust the member only at or after this OpenSSH timestamp
        /// (`YYYYMMDD[Z]` or `YYYYMMDDHHMM[SS][Z]`; append `Z` for UTC).
        #[arg(long, value_name = "TIMESTAMP")]
        valid_after: Option<String>,
        /// Stop trusting the member after this OpenSSH timestamp; omit for trust
        /// that never lapses on its own.
        #[arg(long, value_name = "TIMESTAMP")]
        valid_before: Option<String>,
    },
    /// Remove a member, deleting its ref on a remote and pushing the update.
    Remove {
        /// Member (username) to remove — its `refs/meta/member/<username>` ref.
        username: String,
        /// Remote whose member ref to delete.
        #[arg(default_value = "origin")]
        remote: String,
    },
    /// Revoke a key fast: add its fingerprint to the `refs/meta/revoked` deny
    /// list so it is refused before its window expires, and push the update.
    Revoke {
        /// Fingerprint of the key to deny (as shown by `members list`).
        fingerprint: String,
        /// Remote whose `refs/meta/revoked` to update.
        #[arg(default_value = "origin")]
        remote: String,
        /// Free-text reason recorded alongside the revocation.
        #[arg(long, default_value = "")]
        reason: String,
    },
    /// Lift a revocation, removing a fingerprint from the `refs/meta/revoked`
    /// deny list and pushing the update.
    Unrevoke {
        /// Fingerprint to stop denying.
        fingerprint: String,
        /// Remote whose `refs/meta/revoked` to update.
        #[arg(default_value = "origin")]
        remote: String,
    },
    /// Report whether a key is a member and the client is configured.
    Check {
        /// Remote to read the `refs/meta/member/*` refs from.
        #[arg(default_value = "origin")]
        remote: String,
        /// Key to look for; defaults to `user.signingkey`.
        #[arg(long)]
        key: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum AccountAction {
    /// Create or update this repository's account identity and push it. The
    /// presence of `refs/meta/account` is what marks the repo as an account.
    Create {
        /// The account username — by convention the `user/<username>` repo name.
        username: String,
        /// Remote whose `refs/meta/account` to update.
        #[arg(default_value = "origin")]
        remote: String,
        /// Human-facing display name; defaults to the username.
        #[arg(long)]
        display_name: Option<String>,
        /// Short free-text bio.
        #[arg(long, default_value = "")]
        bio: String,
    },
}

#[derive(Subcommand)]
enum ChecksAction {
    /// List the checks configured on a remote.
    List {
        /// Remote to read `refs/meta/checks` from.
        #[arg(default_value = "origin")]
        remote: String,
    },
    /// Add (or replace) a check on a remote's set and push the update.
    Add {
        /// Name to record the check under (`checks/<name>`).
        name: String,
        /// Command the check runs (e.g. `cargo fmt --check`).
        command: String,
        /// Remote whose `refs/meta/checks` to update.
        #[arg(default_value = "origin")]
        remote: String,
    },
    /// Remove a check from a remote's set and push the update.
    Remove {
        /// Name (`checks/<name>`) to drop.
        name: String,
        /// Remote whose `refs/meta/checks` to update.
        #[arg(default_value = "origin")]
        remote: String,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Top::Members { action } => run_members(action),
        Top::Account { action } => run_account(action),
        Top::Checks { action } => run_checks(action),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run_members(action: Action) -> Result<(), String> {
    match action {
        Action::Setup { key, local } => setup(key.as_deref(), local),
        Action::List { remote } => members_list(&remote),
        Action::Add {
            username,
            remote,
            key,
            cert_authority,
            valid_after,
            valid_before,
        } => members_add(
            &username,
            &remote,
            key.as_deref(),
            cert_authority.as_deref(),
            valid_after,
            valid_before,
        ),
        Action::Remove { username, remote } => members_remove(&username, &remote),
        Action::Revoke {
            fingerprint,
            remote,
            reason,
        } => members_revoke(&fingerprint, &remote, reason),
        Action::Unrevoke {
            fingerprint,
            remote,
        } => members_unrevoke(&fingerprint, &remote),
        Action::Check { remote, key } => check(&remote, key.as_deref()),
    }
}

fn run_account(action: AccountAction) -> Result<(), String> {
    match action {
        AccountAction::Create {
            username,
            remote,
            display_name,
            bio,
        } => account_create(&username, &remote, display_name, bio),
    }
}

fn run_checks(action: ChecksAction) -> Result<(), String> {
    match action {
        ChecksAction::List { remote } => list::<Checks>(&remote),
        ChecksAction::Add {
            name,
            command,
            remote,
        } => add_check(&name, &command, &remote),
        ChecksAction::Remove { name, remote } => remove::<Checks>(&name, &remote),
    }
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
        item.command.clone()
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
/// recorded under that name, and push the update.
fn add_check(name: &str, command: &str, remote: &str) -> Result<(), String> {
    let repo = repo()?;
    let expected = sync(remote, CHECKS_REF)?;
    let mut checks = checks::load(&repo).map_err(|error| error.to_string())?;
    checks.retain(|check| check.name != name);
    checks.push(Check {
        name: name.to_owned(),
        command: command.to_owned(),
    });
    checks::store(&repo, &checks).map_err(|error| error.to_string())?;
    push_signed(remote, CHECKS_REF, expected.as_deref())?;
    println!("recorded check {name}");
    Ok(())
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
    let members = signers::load_all(&repo).map_err(|error| error.to_string())?;
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

/// Authorize a key (or pin a CA) for the member `username` on `remote`, trusting
/// the member within the given validity window, and push the updated member ref.
fn members_add(
    username: &str,
    remote: &str,
    key: Option<&Path>,
    cert_authority: Option<&Path>,
    valid_after: Option<String>,
    valid_before: Option<String>,
) -> Result<(), String> {
    if let Some(after) = &valid_after {
        validate_timestamp(after)?;
    }
    if let Some(before) = &valid_before {
        validate_timestamp(before)?;
    }
    let repo = repo()?;
    let refname = member_ref(username);
    let expected = sync(remote, &refname)?;
    let mut member = signers::load(&repo, username)
        .map_err(|error| error.to_string())?
        .unwrap_or_else(|| Member::with_keys(username.to_owned(), BTreeMap::new()));
    if valid_after.is_some() {
        member.valid_after = valid_after;
    }
    if valid_before.is_some() {
        member.valid_before = valid_before;
    }

    // Pinning a CA replaces the member's trust wholesale — a member is either
    // leaf keys or a CA, never both.
    if let Some(ca_path) = cert_authority {
        let ca = read_public_key(ca_path)?;
        member.trust = Trust::CertAuthority(ca);
        signers::store(&repo, &member).map_err(|error| error.to_string())?;
        push_signed(remote, &refname, expected.as_deref())?;
        println!("pinned a certificate authority for {username}");
        return Ok(());
    }

    let public_key = public_key(key)?;
    let fingerprint = fingerprint(&public_key)?;
    let keys = match &mut member.trust {
        Trust::Keys(keys) => keys,
        Trust::CertAuthority(_ca) => {
            return Err(format!(
                "{username} is pinned to a certificate authority; \
                 revoke and re-add to switch to leaf keys"
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
    signers::store(&repo, &member).map_err(|error| error.to_string())?;
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
    username: &str,
    remote: &str,
    display_name: Option<String>,
    bio: String,
) -> Result<(), String> {
    let repo = repo()?;
    let expected = sync(remote, account::ACCOUNT_REF)?;
    let existing = account::load(&repo).map_err(|error| error.to_string())?;
    let account = Account {
        username: username.to_owned(),
        display_name: display_name.unwrap_or_else(|| username.to_owned()),
        bio,
        // Preserve the original creation time when updating an existing account.
        created_at: existing.map_or_else(now_seconds, |account| account.created_at),
    };
    account::store(&repo, &account).map_err(|error| error.to_string())?;
    push_signed(remote, account::ACCOUNT_REF, expected.as_deref())?;
    println!("created account {username}");
    Ok(())
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

/// Check that `value` is an OpenSSH `allowed_signers` timestamp: `YYYYMMDD`,
/// `YYYYMMDDHHMM`, or `YYYYMMDDHHMMSS`, each optionally suffixed `Z` for UTC.
/// Without `Z` the verifying server reads it in its own local time zone.
fn validate_timestamp(value: &str) -> Result<(), String> {
    let digits = value.strip_suffix('Z').unwrap_or(value);
    let well_formed =
        matches!(digits.len(), 8 | 12 | 14) && digits.bytes().all(|b| b.is_ascii_digit());
    if well_formed {
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
    let members = signers::load_all(&repo).map_err(|error| error.to_string())?;
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
    let listing = git_capture(&["ls-remote", remote, refname])?;
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
