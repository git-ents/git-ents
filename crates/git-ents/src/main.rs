//! `git ents` — the git-ents command-line porcelain.
//!
//! Today it carries a single command, `git ents members`, for managing the
//! repository members recorded at `refs/meta/members` and for configuring this
//! client to produce the signed pushes the server requires. The member commands
//! read and write a remote's set by fetching `refs/meta/members` into the local
//! repository, editing it through [`git_ents::signers`], and pushing it back.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use clap::{Parser, Subcommand};
use git_ents::checks::{self, CHECKS_REF, Check};
use git_ents::signers::{self, MEMBERS_REF, Signer};
use git_store::Row as _;

#[derive(Parser)]
#[command(name = "git-ents", about = "Helpful guardians of your git trees.")]
struct Cli {
    #[command(subcommand)]
    command: Top,
}

#[derive(Subcommand)]
enum Top {
    /// Manage the repository members at `refs/meta/members`.
    Members {
        #[command(subcommand)]
        action: Action,
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
        /// Remote to read `refs/meta/members` from.
        #[arg(default_value = "origin")]
        remote: String,
    },
    /// Add a member to a remote's set and push the update.
    Add {
        /// Remote whose `refs/meta/members` to update.
        #[arg(default_value = "origin")]
        remote: String,
        /// Key to authorize; defaults to `user.signingkey`.
        #[arg(long)]
        key: Option<PathBuf>,
    },
    /// Remove a member from a remote's set and push the update.
    Remove {
        /// Fingerprint (`members/<name>`) to drop.
        fingerprint: String,
        /// Remote whose `refs/meta/members` to update.
        #[arg(default_value = "origin")]
        remote: String,
    },
    /// Report whether a key is a member and the client is configured.
    Check {
        /// Remote to read `refs/meta/members` from.
        #[arg(default_value = "origin")]
        remote: String,
        /// Key to look for; defaults to `user.signingkey`.
        #[arg(long)]
        key: Option<PathBuf>,
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
        Action::List { remote } => list::<Signers>(&remote),
        Action::Add { remote, key } => add(&remote, key.as_deref()),
        Action::Remove {
            fingerprint,
            remote,
        } => remove::<Signers>(&fingerprint, &remote),
        Action::Check { remote, key } => check(&remote, key.as_deref()),
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
/// and pushed to a remote, holding [`git_store::Row`] entries the CLI lists and
/// removes from. The two sets — authorized signers and configured checks —
/// share that flow and differ only in their row type and what the messages call
/// an entry; the thin [`load`](Set::load)/[`store`](Set::store) keep each on its
/// own typed module.
trait Set {
    /// The set's row type, a `(key, value)` pair under [`git_store::Row`].
    type Item: git_store::Row;
    /// The ref the set lives on.
    const REF: &'static str;
    /// The singular noun used in messages ("member", "check").
    const NOUN: &'static str;

    /// The set's rows.
    fn load(repo: &Path) -> Result<Vec<Self::Item>, String>;
    /// Replace the set with `items`.
    fn store(repo: &Path, items: &[Self::Item]) -> Result<(), String>;
    /// The line printed when the set is empty on `remote`.
    fn empty_listing(remote: &str) -> String;
    /// The value column for a row, given its key and stored value.
    fn row_value(key: &str, value: &str) -> String;
}

/// The repository member set at `refs/meta/members`.
struct Signers;

impl Set for Signers {
    type Item = Signer;
    const REF: &'static str = MEMBERS_REF;
    const NOUN: &'static str = "member";

    fn load(repo: &Path) -> Result<Vec<Signer>, String> {
        signers::load(repo).map_err(|error| error.to_string())
    }

    fn store(repo: &Path, items: &[Signer]) -> Result<(), String> {
        signers::store(repo, items).map_err(|error| error.to_string())
    }

    fn empty_listing(remote: &str) -> String {
        format!("no members on {remote} (open bootstrap window)")
    }

    fn row_value(_key: &str, value: &str) -> String {
        key_comment(value)
    }
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

    fn row_value(_key: &str, value: &str) -> String {
        value.to_owned()
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
        let (key, value) = item.into_pair();
        println!("{key}  {}", S::row_value(&key, &value));
    }
    Ok(())
}

/// Drop the entry keyed `key` from the set `S` on `remote` and push the update.
fn remove<S: Set>(key: &str, remote: &str) -> Result<(), String> {
    let repo = repo()?;
    let expected = sync(remote, S::REF)?;
    let before: Vec<(String, String)> = S::load(&repo)?
        .into_iter()
        .map(git_store::Row::into_pair)
        .collect();
    let count = before.len();
    let after: Vec<S::Item> = before
        .into_iter()
        .filter(|(k, _v)| k != key)
        .map(|(k, v)| S::Item::from_pair(k, v))
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

/// Authorize `key` on `remote` and push the updated set.
fn add(remote: &str, key: Option<&Path>) -> Result<(), String> {
    let repo = repo()?;
    let public_key = public_key(key)?;
    let fingerprint = fingerprint(&public_key)?;
    let expected = sync(remote, MEMBERS_REF)?;
    let mut signers = signers::load(&repo).map_err(|error| error.to_string())?;
    if signers
        .iter()
        .any(|signer| same_key(&signer.key, &public_key))
    {
        println!("{fingerprint} is already a member");
        return Ok(());
    }
    signers.push(Signer {
        fingerprint: fingerprint.clone(),
        key: public_key,
    });
    signers::store(&repo, &signers).map_err(|error| error.to_string())?;
    push_signed(remote, MEMBERS_REF, expected.as_deref())?;
    println!("authorized {fingerprint}");
    Ok(())
}

/// Report whether `key` is in `remote`'s set and how this client is configured.
fn check(remote: &str, key: Option<&Path>) -> Result<(), String> {
    let repo = repo()?;
    let public_key = public_key(key)?;
    let fingerprint = fingerprint(&public_key)?;
    sync(remote, MEMBERS_REF)?;
    let signers = signers::load(&repo).map_err(|error| error.to_string())?;
    if signers.is_empty() {
        println!("{remote}: open bootstrap window (no members yet)");
    } else if signers
        .iter()
        .any(|signer| same_key(&signer.key, &public_key))
    {
        println!("{remote}: {fingerprint} is a member");
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
