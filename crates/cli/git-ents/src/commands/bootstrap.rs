//! `git ents bootstrap`: an operator's first-boot enrollment of a fresh
//! hosted root, run from a clone — never on the server.
//!
//! Order is the whole design: the operator's own key lands first, as the
//! self-admitting first push (`gate.bootstrap`), and the server's key is
//! then vouched for under the operator's signature (`roots.web-signing`)
//! so `serve --hosted`'s fail-closed web UI can boot. Enrolling
//! server-side instead would spend the self-admitting push on the
//! server's own key, making the machine the trust root rather than the
//! operator — the reason `docker/entrypoint.sh` retries and waits for
//! this command instead of enrolling itself.
//!
//! The server key to vouch for is discovered, not copied: the front
//! proxy serves the key's public half at `/.ents/server-key` (written by
//! `git ents setup --hosted`) precisely while the web UI waits for this
//! enrollment. `--server-pubkey` overrides discovery for a remote that
//! is not http(s). Only the public half is ever fetched; the private key
//! never leaves the server's volume, in either direction.

use std::path::PathBuf;
use std::process::Command;

use crate::commands::members;
use crate::error::{Error, Result};
use crate::root::LocalRoot;

/// Enroll `username` (the operator, signed by `key` or `user.signingkey`)
/// and then `server_name` holding `server_pubkey` — discovered from
/// `remote` when not given — pushing each enrollment to `remote` as it
/// lands.
///
/// # Errors
///
/// [`Error::NotFound`] if discovery is needed but `remote` is not an
/// http(s) URL or does not answer with a public key; see [`members::add`]
/// for an enrollment failure; [`Error::Push`] if a push is refused or the
/// transport fails. Discovery and the operator's push both precede the
/// server enrollment, so a failure stops the bootstrap with nothing
/// half-done on the remote.
pub fn run(
    root: &LocalRoot,
    username: &str,
    server_pubkey: Option<String>,
    server_name: &str,
    remote: &str,
    key: Option<PathBuf>,
    out: &mut impl std::io::Write,
) -> Result<()> {
    let server_pubkey = match server_pubkey {
        Some(given) => given,
        None => {
            let discovered = discover_server_pubkey(root, remote)?;
            let _ = writeln!(out, "discovered server key: {discovered}");
            discovered
        }
    };
    members::add(root, username, None, key.clone())?;
    push(root, remote, username)?;
    let _ = writeln!(out, "enrolled {username} (self-admitting first push)");
    members::add(root, server_name, Some(server_pubkey), key)?;
    push(root, remote, server_name)?;
    let _ = writeln!(
        out,
        "enrolled {server_name} (server key, vouched for by {username})"
    );
    Ok(())
}

/// Push `username`'s member ref to `remote` via a real `git push`, so the
/// remote's own hooks gate the enrollment exactly as any other push.
fn push(root: &LocalRoot, remote: &str, username: &str) -> Result<()> {
    let refspec = format!("refs/meta/member/{username}");
    let output = Command::new("git")
        .arg("-C")
        .arg(&root.path)
        .args(["push", remote, &refspec])
        .output()
        .map_err(|source| Error::Io {
            path: root.path.clone(),
            source,
        })?;
    if !output.status.success() {
        return Err(Error::Push {
            refspec,
            remote: remote.to_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(())
}

/// Fetch the server's public key from `remote`'s host at
/// `/.ents/server-key`, the path the hosted root's front proxy serves it
/// on while the web UI is fail-closed.
fn discover_server_pubkey(root: &LocalRoot, remote: &str) -> Result<String> {
    let url = remote_url(root, remote)?;
    let base = http_origin(&url).ok_or_else(|| Error::NotFound {
        what: format!(
            "an http(s) remote to discover the server key from ({remote} is {url}); \
             pass --server-pubkey instead"
        ),
    })?;
    let endpoint = format!("{base}/.ents/server-key");
    let body = ureq::get(&endpoint)
        .call()
        .map_err(|source| Error::NotFound {
            what: format!("the server key at {endpoint}: {source}"),
        })?
        .body_mut()
        .read_to_string()
        .map_err(|source| Error::NotFound {
            what: format!("the server key at {endpoint}: {source}"),
        })?;
    let pubkey = body.trim();
    if !pubkey.starts_with("ssh-") {
        return Err(Error::NotFound {
            what: format!(
                "an OpenSSH public key at {endpoint} — is this a git-ents hosted root awaiting \
                 bootstrap?"
            ),
        });
    }
    Ok(pubkey.to_owned())
}

/// `remote`'s configured URL, via `git remote get-url` so `insteadOf`
/// rewrites apply exactly as they would to the pushes that follow.
fn remote_url(root: &LocalRoot, remote: &str) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(&root.path)
        .args(["remote", "get-url", remote])
        .output()
        .map_err(|source| Error::Io {
            path: root.path.clone(),
            source,
        })?;
    if !output.status.success() {
        return Err(Error::NotFound {
            what: format!("a remote named {remote}"),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// The `scheme://host[:port]` prefix of an http(s) URL, or `None` for any
/// other transport (ssh, scp-like, file) — discovery has nowhere to GET
/// from on those.
fn http_origin(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let host = rest.split('/').next()?;
    (!host.is_empty()).then(|| format!("{scheme}://{host}"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, reason = "unit test")]

    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::https("https://git.ents.cloud/repo.git", Some("https://git.ents.cloud"))]
    #[case::port("http://127.0.0.1:8080/repo.git", Some("http://127.0.0.1:8080"))]
    #[case::bare_host("https://git.ents.cloud", Some("https://git.ents.cloud"))]
    #[case::ssh("ssh://git@ents.cloud/repo.git", None)]
    #[case::scp_like("git@github.com:git-ents/git-ents.git", None)]
    #[case::file_path("/data/repo.git", None)]
    fn http_origin_accepts_only_http(#[case] url: &str, #[case] expected: Option<&str>) {
        assert_eq!(http_origin(url).as_deref(), expected);
    }
}
