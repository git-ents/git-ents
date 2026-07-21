//! `git ents setup`: resolve or generate a signing key, record it as this
//! repository's `user.signingkey` with `gpg.format=ssh`, set
//! `receive.denyCurrentBranch=updateInstead` (`roots.worktree-update`),
//! and ([`configure_global_signing_defaults`]) default every commit, tag,
//! and (when asked) push, in any repository, to sign itself.
//!
//! `receive.denyCurrentBranch=updateInstead` is the integration-test
//! harness edge case `roots.worktree-update` names: it lets an external
//! push land on this repository's checked-out branch and still update the
//! working tree, which is not how a normal git remote behaves and is never
//! needed for `refs/meta/*` traffic (which never touches a worktree at
//! all).
//!
//! `--hosted` ([`run_hosted`]) configures the single-node hosted root
//! instead (`roots.single-node-hosted`): a signing key for the hosted
//! worker, and this binary's own `hook pre-receive`/`hook post-receive`
//! installed into a bare repository's `hooks/`. Without this, a hosted
//! bare repository accepts every push completely ungated — stock git's
//! `receive-pack` enforces nothing on its own; the gate exists only where
//! a hook calls it.

use std::path::{Path, PathBuf};
use std::process::Command;

use rand_core::{OsRng, RngCore as _};
use ssh_key::{Algorithm, LineEnding, PrivateKey};

use crate::error::{Error, Result};
use crate::root::LocalRoot;
use crate::sign::Signer;

/// Run `git ents setup` against `root`: resolve `key`, generating a new
/// `~/.ssh/id_ed25519` if neither `key` nor `user.signingkey` resolves to
/// an existing file, then write `user.signingkey`, `gpg.format=ssh`, and
/// `receive.denyCurrentBranch=updateInstead` to the repository's own
/// (local) config.
///
/// # Errors
///
/// [`Error::BadSigningKey`] if a given or configured key cannot be loaded;
/// [`Error::Io`] if generating or writing a new key fails; propagates a
/// config-write failure.
pub fn run(root: &LocalRoot, key: Option<PathBuf>) -> Result<PathBuf> {
    let resolved = resolve_or_generate_key(&root.path, key)?;
    let path_str = resolved.to_string_lossy().into_owned();
    for (key, value) in [
        ("user.signingkey", path_str.as_str()),
        ("gpg.format", "ssh"),
        ("receive.denyCurrentBranch", "updateInstead"),
    ] {
        set_local_config(&root.path, key, value)?;
    }
    Ok(resolved)
}

/// Run `git ents setup --hosted` against the bare repository at `path`:
/// resolve or generate a signing key for the hosted worker (recorded as
/// `path`'s own `user.signingkey`/`gpg.format=ssh`, same as [`run`]),
/// install this binary's `hook pre-receive`/`hook post-receive` as
/// `path`'s own git hooks (`roots.single-node-hosted`), and require every
/// push to carry a verifiable signed-push certificate (below).
///
/// `receive.denyCurrentBranch=updateInstead` is deliberately not set here:
/// it is the local-root, checked-out-worktree edge case
/// (`roots.worktree-update`), meaningless for a bare repository with no
/// worktree to update.
///
/// # Errors
///
/// [`Error::BadSigningKey`] if a given or configured key cannot be loaded;
/// [`Error::Io`] if generating a key, writing config, resolving this
/// binary's own path, or writing a hook file fails.
// @relation(roots.single-node-hosted, scope=function)
pub fn run_hosted(path: &Path, key: Option<PathBuf>) -> Result<PathBuf> {
    let resolved = resolve_or_generate_key(path, key)?;
    let path_str = resolved.to_string_lossy().into_owned();
    for (key, value) in [
        ("user.signingkey", path_str.as_str()),
        ("gpg.format", "ssh"),
    ] {
        set_local_config(path, key, value)?;
    }
    write_pubkey(&resolved)?;
    install_hooks(path)?;
    configure_signed_push(path)?;
    Ok(resolved)
}

/// Make `receive-pack` advertise and require the `push-cert` capability:
/// without `receive.certNonceSeed` set, stock git never advertises it at
/// all, so `git push --signed` fails with "the receiving end does not
/// support --signed push" regardless of what `hook pre_receive` goes on
/// to verify. The seed itself only needs to stay stable across the two
/// requests one push makes (the capability advertisement and the push
/// itself) — not across pushes — but is generated once and reused on
/// every later boot anyway, so an in-flight push spanning a restart still
/// verifies. `certNonceSlop` tolerates the two requests landing in
/// different seconds.
fn configure_signed_push(path: &Path) -> Result<()> {
    let seed = match get_local_config(path, "receive.certNonceSeed")? {
        Some(existing) => existing,
        None => generate_nonce_seed(),
    };
    for (key, value) in [
        ("receive.certNonceSeed", seed.as_str()),
        ("receive.certNonceSlop", "60"),
    ] {
        set_local_config(path, key, value)?;
    }
    Ok(())
}

/// 32 random bytes, hex-encoded — plenty of entropy for a nonce-signing
/// secret that never leaves this repository's local config.
fn generate_nonce_seed() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Write the key's public half to `<key>.pub` — the front proxy publishes
/// it at a well-known path so `git ents bootstrap` can discover the server
/// identity to vouch for (`roots.web-signing`) without the operator
/// copying it out of the logs. Runs every boot, so a volume whose key
/// predates this file gains it on the next deploy.
fn write_pubkey(key: &Path) -> Result<()> {
    let pubkey = Signer::load(key)?.public_openssh();
    let pub_path = PathBuf::from(format!("{}.pub", key.display()));
    std::fs::write(&pub_path, format!("{pubkey}\n")).map_err(|source| Error::Io {
        path: pub_path,
        source,
    })
}

/// Resolve `key` (or `path`'s `user.signingkey`, or a default
/// `~/.ssh/id_ed25519`), generating a fresh key if nothing resolves to an
/// existing file, and confirm the result actually loads.
fn resolve_or_generate_key(path: &Path, key: Option<PathBuf>) -> Result<PathBuf> {
    let repo = gix::open(path)?;
    let resolved = match crate::sign::resolve_key_path(&repo, key.as_deref()) {
        Ok(candidate) if candidate.exists() => candidate,
        Ok(candidate) => generate_key(&candidate)?,
        Err(Error::NoSigningKey) => {
            let default = default_key_path()?;
            generate_key(&default)?
        }
        Err(other) => return Err(other),
    };
    // Confirm the resolved key actually loads before recording it.
    Signer::load(&resolved)?;
    Ok(resolved)
}

/// Install this binary's own `hook pre-receive`/`hook post-receive` as
/// `repo_path`'s git hooks, overwriting any existing scripts of the same
/// name — the mechanism `roots.single-node-hosted` requires: without
/// these hooks, git's own `receive-pack` performs no gate check at all,
/// and a hosted bare repository would accept every push ungated.
///
/// # Errors
///
/// [`Error::Io`] if this binary's own path cannot be resolved, the
/// `hooks/` directory cannot be created, or a hook file cannot be written
/// or (on unix) made executable.
fn install_hooks(repo_path: &Path) -> Result<()> {
    let this_binary = std::env::current_exe().map_err(|source| Error::Io {
        path: repo_path.to_owned(),
        source,
    })?;
    let hooks_dir = repo_path.join("hooks");
    std::fs::create_dir_all(&hooks_dir).map_err(|source| Error::Io {
        path: hooks_dir.clone(),
        source,
    })?;
    for hook in ["pre-receive", "post-receive"] {
        let script = format!("#!/bin/sh\nexec {:?} hook {hook}\n", this_binary.display());
        let hook_path = hooks_dir.join(hook);
        std::fs::write(&hook_path, script).map_err(|source| Error::Io {
            path: hook_path.clone(),
            source,
        })?;
        set_executable(&hook_path)?;
    }
    Ok(())
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let mut perms = std::fs::metadata(path)
        .map_err(|source| Error::Io {
            path: path.to_owned(),
            source,
        })?
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}

/// Sign every commit, tag, and (when the remote asks) push by default —
/// written to the operator's *global* (`~/.gitconfig`) config, not any
/// one repository's, so `git ents setup` needs running only once per
/// machine for every later `git commit`/`git push`, anywhere, to sign
/// itself without `-S`/`--signed`. `push.gpgsign=if-asked` in particular
/// is what makes a plain `git push` safe against both a hosted root
/// (which now advertises `push-cert`, so this signs) and a remote that
/// does not (GitHub among them, which never has — this silently pushes
/// unsigned there instead of failing outright).
///
/// Deliberately not called from [`run`] itself: `run`'s callers configure
/// one *repository* (this crate's own integration tests among them), and
/// must never mutate the real machine's global git config as a side
/// effect of that; only the actual `git ents setup` CLI invocation calls
/// this.
///
/// # Errors
///
/// Propagates a `git config --global` failure.
pub fn configure_global_signing_defaults() -> Result<()> {
    for (key, value) in [
        ("commit.gpgsign", "true"),
        ("tag.gpgsign", "true"),
        ("push.gpgsign", "if-asked"),
    ] {
        set_global_config(key, value)?;
    }
    Ok(())
}

/// Set `key` to `value` in the operator's global (`~/.gitconfig`) config
/// via `git config --global` — unlike [`set_local_config`], not scoped to
/// any one repository.
fn set_global_config(key: &str, value: &str) -> Result<()> {
    let output = Command::new("git")
        .args(["config", "--global", key, value])
        .output()
        .map_err(|source| Error::Io {
            path: PathBuf::from("~/.gitconfig"),
            source,
        })?;
    if !output.status.success() {
        return Err(Error::Io {
            path: PathBuf::from("~/.gitconfig"),
            source: std::io::Error::other(format!(
                "git config --global {key} {value} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )),
        });
    }
    Ok(())
}

/// Set `key` to `value` in `repo_path`'s own local config via `git config`.
fn set_local_config(repo_path: &Path, key: &str, value: &str) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["config", "--local", key, value])
        .output()
        .map_err(|source| Error::Io {
            path: repo_path.to_owned(),
            source,
        })?;
    if !output.status.success() {
        return Err(Error::BadSigningKey {
            path: repo_path.to_owned(),
            detail: format!(
                "git config --local {key} {value} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ),
        });
    }
    Ok(())
}

/// Read `key` from `repo_path`'s own local config, or `None` if it is
/// unset — used to make [`configure_signed_push`] idempotent across boots
/// rather than mint a fresh nonce seed (and so a fresh push-cert
/// namespace) every deploy.
fn get_local_config(repo_path: &Path, key: &str) -> Result<Option<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["config", "--local", "--get", key])
        .output()
        .map_err(|source| Error::Io {
            path: repo_path.to_owned(),
            source,
        })?;
    if !output.status.success() {
        return Ok(None);
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok((!value.is_empty()).then_some(value))
}

/// Generate a fresh, unencrypted ed25519 key at `path` (creating parent
/// directories as needed) and return `path` unchanged.
fn generate_key(path: &Path) -> Result<PathBuf> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| Error::Io {
            path: parent.to_owned(),
            source,
        })?;
    }
    let key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).map_err(|source| {
        Error::BadSigningKey {
            path: path.to_owned(),
            detail: source.to_string(),
        }
    })?;
    key.write_openssh_file(path, LineEnding::LF)
        .map_err(|source| Error::BadSigningKey {
            path: path.to_owned(),
            detail: source.to_string(),
        })?;
    Ok(path.to_owned())
}

fn default_key_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or(Error::NoSigningKey)?;
    Ok(PathBuf::from(home).join(".ssh").join("id_ed25519"))
}
