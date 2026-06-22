//! `git ents` — the git-ents command-line porcelain.
//!
//! Today it carries a single command, `git ents auth`, for managing the
//! authorized push signers recorded at `refs/meta/auth` and for configuring
//! this client to produce the signed pushes the server requires. The signer
//! commands read and write a remote's set by fetching `refs/meta/auth` into the
//! local repository, editing it through [`git_ents::signers`], and pushing it
//! back.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use clap::{Parser, Subcommand};
use git_ents::signers::{self, AUTH_REF, Signer};

#[derive(Parser)]
#[command(name = "git-ents", about = "Helpful guardians of your git trees.")]
struct Cli {
    #[command(subcommand)]
    command: Top,
}

#[derive(Subcommand)]
enum Top {
    /// Manage the authorized push signers at `refs/meta/auth`.
    Auth {
        #[command(subcommand)]
        action: Action,
    },
}

#[derive(Subcommand)]
enum Action {
    /// Configure this client to sign the pushes the server requires.
    Configure {
        /// Key to sign with; defaults to `user.signingkey`.
        #[arg(long)]
        key: Option<PathBuf>,
        /// Write to global git config instead of this repository's.
        #[arg(long)]
        global: bool,
    },
    /// List the authorized signers on a remote.
    List {
        /// Remote to read `refs/meta/auth` from.
        #[arg(default_value = "origin")]
        remote: String,
    },
    /// Add a signer to a remote's set and push the update.
    Add {
        /// Remote whose `refs/meta/auth` to update.
        #[arg(default_value = "origin")]
        remote: String,
        /// Key to authorize; defaults to `user.signingkey`.
        #[arg(long)]
        key: Option<PathBuf>,
    },
    /// Remove a signer from a remote's set and push the update.
    Remove {
        /// Fingerprint (`signers/<name>`) to drop.
        fingerprint: String,
        /// Remote whose `refs/meta/auth` to update.
        #[arg(default_value = "origin")]
        remote: String,
    },
    /// Report whether a key is authorized and the client is configured.
    Check {
        /// Remote to read `refs/meta/auth` from.
        #[arg(default_value = "origin")]
        remote: String,
        /// Key to look for; defaults to `user.signingkey`.
        #[arg(long)]
        key: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Top::Auth { action } => run_auth(action),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run_auth(action: Action) -> Result<(), String> {
    match action {
        Action::Configure { key, global } => configure(key.as_deref(), global),
        Action::List { remote } => list(&remote),
        Action::Add { remote, key } => add(&remote, key.as_deref()),
        Action::Remove {
            fingerprint,
            remote,
        } => remove(&fingerprint, &remote),
        Action::Check { remote, key } => check(&remote, key.as_deref()),
    }
}

/// Set the local (or global) git config that makes `git push` sign for this
/// server: SSH-format signatures, a signing key, and "sign when the server
/// asks" so pushes elsewhere are untouched.
fn configure(key: Option<&Path>, global: bool) -> Result<(), String> {
    let scope = if global { "--global" } else { "--local" };
    set_config(scope, "gpg.format", "ssh")?;
    set_config(scope, "push.gpgSign", "if-asked")?;
    match key {
        Some(path) => set_config(scope, "user.signingkey", &path.display().to_string())?,
        None => {
            if config_get("user.signingkey").is_none() {
                eprintln!(
                    "note: user.signingkey is unset; pass --key <path> or set it to sign pushes"
                );
            }
        }
    }
    println!(
        "configured signed pushes ({} git config)",
        scope.trim_start_matches('-')
    );
    Ok(())
}

/// Print each authorized signer on `remote` as `<fingerprint>  <comment>`.
fn list(remote: &str) -> Result<(), String> {
    let repo = repo()?;
    sync_auth(remote)?;
    let signers = signers::load(&repo).map_err(|error| error.to_string())?;
    if signers.is_empty() {
        println!("no authorized signers on {remote} (open bootstrap window)");
        return Ok(());
    }
    for signer in &signers {
        println!("{}  {}", signer.fingerprint, key_comment(&signer.key));
    }
    Ok(())
}

/// Authorize `key` on `remote` and push the updated set.
fn add(remote: &str, key: Option<&Path>) -> Result<(), String> {
    let repo = repo()?;
    let public_key = public_key(key)?;
    let fingerprint = fingerprint(&public_key)?;
    sync_auth(remote)?;
    let mut signers = signers::load(&repo).map_err(|error| error.to_string())?;
    if signers
        .iter()
        .any(|signer| same_key(&signer.key, &public_key))
    {
        println!("{fingerprint} is already authorized");
        return Ok(());
    }
    signers.push(Signer {
        fingerprint: fingerprint.clone(),
        key: public_key,
    });
    signers::store(&repo, &signers).map_err(|error| error.to_string())?;
    push_auth(remote)?;
    println!("authorized {fingerprint}");
    Ok(())
}

/// Drop the signer named `fingerprint` from `remote` and push the update.
fn remove(fingerprint: &str, remote: &str) -> Result<(), String> {
    let repo = repo()?;
    sync_auth(remote)?;
    let before = signers::load(&repo).map_err(|error| error.to_string())?;
    let count = before.len();
    let after: Vec<Signer> = before
        .into_iter()
        .filter(|signer| signer.fingerprint != fingerprint)
        .collect();
    if after.len() == count {
        return Err(format!("no signer named {fingerprint} on {remote}"));
    }
    signers::store(&repo, &after).map_err(|error| error.to_string())?;
    push_auth(remote)?;
    println!("removed {fingerprint}");
    Ok(())
}

/// Report whether `key` is in `remote`'s set and how this client is configured.
fn check(remote: &str, key: Option<&Path>) -> Result<(), String> {
    let repo = repo()?;
    let public_key = public_key(key)?;
    let fingerprint = fingerprint(&public_key)?;
    sync_auth(remote)?;
    let signers = signers::load(&repo).map_err(|error| error.to_string())?;
    if signers.is_empty() {
        println!("{remote}: open bootstrap window (no signers yet)");
    } else if signers
        .iter()
        .any(|signer| same_key(&signer.key, &public_key))
    {
        println!("{remote}: {fingerprint} is authorized");
    } else {
        println!("{remote}: {fingerprint} is NOT authorized");
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

/// Mirror `remote`'s `refs/meta/auth` into the local repository so the signer
/// helpers see the current set. When the remote has none, clear any stale local
/// ref so the set reads empty (the open bootstrap window).
fn sync_auth(remote: &str) -> Result<(), String> {
    let listing = git_capture(&["ls-remote", remote, AUTH_REF])?;
    if listing.trim().is_empty() {
        let _deleted = git_capture(&["update-ref", "-d", AUTH_REF]);
        Ok(())
    } else {
        let refspec = format!("+{AUTH_REF}:{AUTH_REF}");
        git_run(&["fetch", "--quiet", remote, &refspec])
    }
}

/// Push the local `refs/meta/auth` to `remote`, signed per the client's config.
fn push_auth(remote: &str) -> Result<(), String> {
    git_run(&["push", remote, AUTH_REF])
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
/// would split the `signers/<name>` entry into a subtree.
fn fingerprint(public_key: &str) -> Result<String, String> {
    let scratch = Scratch::new()?;
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

/// A uniquely named temporary directory removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Result<Self, String> {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("git-ents-cli-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir)
            .map_err(|error| format!("could not create temp dir: {error}"))?;
        Ok(Self(dir))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        match std::fs::remove_dir_all(&self.0) {
            Ok(()) | Err(_) => {}
        }
    }
}
