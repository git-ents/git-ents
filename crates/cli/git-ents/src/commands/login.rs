//! `git ents login <url> <code>`: prove membership to a hosted web
//! session (`roots.web-signin`) — the automated replacement for the
//! pre-redo forge's paste-the-signature sign-in.
//!
//! The browser's `/login` page on the hosted root displays a one-time
//! code; this command fetches that code's challenge, **rebuilds the
//! signed payload locally** from the host the member typed — never from
//! bytes the server supplies, so a malicious or misconfigured server
//! cannot get an arbitrary blob signed — signs it with the member's own
//! key under `ents-web`'s login namespace (distinct from git's commit
//! namespace by construction), and posts the signature back. On success
//! the browser session that displayed the code is signed in; nothing
//! secret ever leaves this machine.
//!
//! The HTTP client is `ureq`: synchronous (two requests need no runtime)
//! and rustls/ring, so the hosted root's musl static release cross-build
//! keeps working.
// @relation(roots.web-signin, scope=file)

use std::path::PathBuf;

use ents_web::auth;

use crate::error::{Error, Result};
use crate::root::LocalRoot;
use crate::sign::Signer;

/// Run the sign-in: resolve the member key exactly as every mutation
/// command does when run inside a repository (`--key`, else
/// `user.signingkey`, else `~/.ssh/id_ed25519`), falling back to the
/// same non-repository chain when run from anywhere else — a login
/// targets a *hosted* root, so requiring a local clone would be
/// arbitrary.
///
/// # Errors
///
/// [`Error::NoSigningKey`]/[`Error::BadSigningKey`] resolving the key;
/// [`Error::NotFound`] for an unknown or expired code, a host mismatch
/// between `url` and the server's own answer, or a server that refuses
/// the signature (each with the server's own explanation).
// @relation(roots.web-signin, scope=function)
pub fn run(
    url: &str,
    code: &str,
    key: Option<PathBuf>,
    mut out: impl std::io::Write,
) -> Result<()> {
    let signer = resolve_signer(key)?;
    let url = url.trim_end_matches('/');
    let host = host_of(url)?;
    let code = auth::normalize_code(code);

    let agent = ureq::agent();
    let challenge = agent
        .get(format!("{url}/login/challenge/{code}"))
        .call()
        .map_err(|source| http_error(url, &source))?
        .body_mut()
        .read_to_string()
        .map_err(|source| http_error(url, &source))?;
    let served_host = line_value(&challenge, "host");
    let nonce = line_value(&challenge, "nonce");
    let (Some(served_host), Some(nonce)) = (served_host, nonce) else {
        return Err(Error::NotFound {
            what: format!("a challenge in {url}'s answer — is this a git-ents hosted root?"),
        });
    };
    // The typed URL is the trust anchor (`roots.web-signin`): a server
    // answering for a different host gets nothing signed.
    if served_host != host {
        return Err(Error::NotFound {
            what: format!(
                "host agreement: you addressed {host}, the server answers for {served_host}"
            ),
        });
    }

    let payload = auth::challenge_payload(&host, &code, nonce);
    let signature = signer.sign_in_namespace(auth::LOGIN_NAMESPACE, payload.as_bytes());
    let public_key = signer.public_openssh();
    let _ = writeln!(out, "proving membership to {url} as {public_key}");

    let response = agent
        .post(format!("{url}/login/challenge/{code}"))
        .send_form([
            ("public_key", public_key.as_str()),
            ("signature", signature.as_str()),
        ]);
    match response {
        Ok(mut response) => {
            let body = response.body_mut().read_to_string().unwrap_or_default();
            let member = line_value(&body, "member").unwrap_or("<unknown>");
            let _ = writeln!(
                out,
                "signed in: the browser session is now authenticated as {member}"
            );
            Ok(())
        }
        Err(ureq::Error::StatusCode(status)) => Err(Error::NotFound {
            what: match status {
                401 => format!(
                    "membership: {url} refused the signature — is {public_key} enrolled and \
                     active there?"
                ),
                404 | 410 => format!(
                    "a live sign-in code: {code} is unknown, expired, or already used; reload \
                     the sign-in page for a fresh one"
                ),
                other => format!("a sign-in answer from {url} (HTTP {other})"),
            },
        }),
        Err(source) => Err(http_error(url, &source)),
    }
}

/// Resolve the signing key with [`crate::commands::signer`]'s chain when
/// inside a repository, else the same chain minus the repository config.
fn resolve_signer(key: Option<PathBuf>) -> Result<Signer> {
    match LocalRoot::discover(".") {
        Ok(root) => crate::commands::signer(&root, key),
        Err(_not_a_repo) => {
            if let Some(path) = key {
                return Signer::load(&path);
            }
            let home = std::env::var_os("HOME").map(PathBuf::from);
            let default = home
                .map(|home| home.join(".ssh").join("id_ed25519"))
                .filter(|path| path.exists());
            match default {
                Some(path) => Signer::load(&path),
                None => Err(Error::NoSigningKey),
            }
        }
    }
}

/// The host (and non-default port) component of an `https://` or
/// `http://` base URL — the exact string bound into the signed payload.
fn host_of(url: &str) -> Result<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .ok_or_else(|| Error::NotFound {
            what: format!("an http(s):// URL (got {url})"),
        })?;
    let host = rest.split('/').next().unwrap_or_default();
    if host.is_empty() {
        return Err(Error::NotFound {
            what: format!("a host in {url}"),
        });
    }
    Ok(host.to_owned())
}

/// The value of a `key=value` line in the server's plain-text answers.
fn line_value<'a>(body: &'a str, key: &str) -> Option<&'a str> {
    body.lines()
        .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
}

fn http_error(url: &str, source: &dyn std::fmt::Display) -> Error {
    Error::NotFound {
        what: format!("a reachable hosted root at {url}: {source}"),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, reason = "unit test")]

    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::https("https://git.ents.cloud", "git.ents.cloud")]
    #[case::trailing_path("https://git.ents.cloud/x", "git.ents.cloud")]
    #[case::port("http://127.0.0.1:4880", "127.0.0.1:4880")]
    fn host_of_extracts_the_authority(#[case] url: &str, #[case] expected: &str) {
        assert_eq!(host_of(url).expect("parses"), expected);
    }

    #[rstest]
    fn host_of_refuses_a_bare_name() {
        host_of("git.ents.cloud").unwrap_err();
    }

    #[rstest]
    fn line_value_reads_the_servers_plain_answers() {
        let body = "host=ents.test\ncode=ABCD2345\nnonce=00ff\n";
        assert_eq!(line_value(body, "host"), Some("ents.test"));
        assert_eq!(line_value(body, "nonce"), Some("00ff"));
        assert_eq!(line_value(body, "member"), None);
    }
}
