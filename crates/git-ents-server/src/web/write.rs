//! Authenticated browser writes.
//!
//! A web session holds one member's *web key* — a key whose public half they
//! have already added to their member ref through a normal signed push. An edit
//! made in the browser is performed as a real `git push --signed` into the repo,
//! so it travels through the very same `pre-receive` gate a command-line push
//! does. Nothing here is a second trust path: the gate alone decides whether a
//! change lands; this module only stages the change and produces a signed push
//! for it to judge.
//!
//! The web key lives in memory for the life of the process and is never written
//! to disk. A server restart drops every session.

use std::collections::HashMap;
use std::io::Read as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

/// The cookie that carries a session token.
pub(super) const COOKIE: &str = "ents_session";

/// In-memory session table, shared by every handler.
pub(crate) type Sessions = Arc<Mutex<HashMap<String, Session>>>;

/// One browser session: the web key it signs edits with and a display label.
pub(crate) struct Session {
    /// The PEM private key the session signs pushes with. In memory only.
    private_key: String,
    /// The derived public key line (`type base64`), matched against members.
    public_key: String,
    /// A human label for the key — its given name, or its type.
    label: String,
}

/// A cheap, cloneable view of a session for rendering and authorization, without
/// the private key.
#[derive(Clone)]
pub(super) struct SessionSnapshot {
    pub(super) label: String,
    pub(super) public_key: String,
}

/// Create an empty session table.
pub(crate) fn new_sessions() -> Sessions {
    Arc::new(Mutex::new(HashMap::new()))
}

/// The session a `Cookie` header points at, as a snapshot, if any.
pub(super) fn snapshot(sessions: &Sessions, cookie: Option<&str>) -> Option<SessionSnapshot> {
    let token = token(cookie?)?;
    let table = sessions.lock().ok()?;
    let session = table.get(&token)?;
    Some(SessionSnapshot {
        label: session.label.clone(),
        public_key: session.public_key.clone(),
    })
}

/// Open a session for the web key in `body` (a `private_key` form field), set its
/// cookie, and return the token. The key is accepted as long as it parses; an
/// edit is authorized per-repository against the live member list, so holding a
/// session grants nothing on its own.
pub(super) fn login(sessions: &Sessions, body: &[u8]) -> Result<String, String> {
    let fields = form(body);
    let private_key = fields
        .get("private_key")
        .map(String::as_str)
        .unwrap_or_default()
        .trim()
        .to_owned();
    if private_key.is_empty() {
        return Err("paste a private key to sign in".to_owned());
    }
    let public_key = derive_public_key(&private_key)?;
    let label = fields
        .get("label")
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| key_type(&public_key));

    let token = random_token()?;
    let mut table = sessions
        .lock()
        .map_err(|_poisoned| "session store unavailable".to_owned())?;
    table.insert(
        token.clone(),
        Session {
            private_key,
            public_key,
            label,
        },
    );
    Ok(token)
}

/// Drop the session a `Cookie` header points at, if any.
pub(super) fn logout(sessions: &Sessions, cookie: Option<&str>) {
    let Some(token) = cookie.and_then(token) else {
        return;
    };
    if let Ok(mut table) = sessions.lock() {
        table.remove(&token);
    }
}

/// Land a new repository description by staging it on a throwaway ref and pushing
/// it, signed with the session's web key, onto `refs/meta/config` — through the
/// `pre-receive` gate. Returns `Ok` only when the gate accepts the push.
///
/// `seed` and `hooks` are the server's signed-push nonce seed and hooks
/// directory; both are required, so a web edit is never a way around a server
/// that is not enforcing the gate.
pub(super) fn edit_description(
    sessions: &Sessions,
    cookie: Option<&str>,
    repo: &Path,
    new_description: &str,
    seed: &str,
    hooks: &Path,
) -> Result<(), String> {
    let token = cookie
        .and_then(token)
        .ok_or_else(|| "sign in to edit settings".to_owned())?;
    let (private_key, public_key) = {
        let table = sessions
            .lock()
            .map_err(|_poisoned| "session store unavailable".to_owned())?;
        let session = table
            .get(&token)
            .ok_or_else(|| "sign in to edit settings".to_owned())?;
        (session.private_key.clone(), session.public_key.clone())
    };

    let username = member_for_public_key(repo, &public_key)
        .ok_or_else(|| "your web key is not a member of this repository".to_owned())?;

    let mut config =
        git_ents::config::load(repo).map_err(|e| format!("could not read config: {e}"))?;
    config.description = new_description.to_owned();

    let staging = format!("refs/web-staging/{}", random_token()?);
    let result = stage_and_push(
        repo,
        &staging,
        &config,
        &private_key,
        &username,
        seed,
        hooks,
    );
    // Clean up the staging ref whether or not the push was accepted.
    let _cleanup = git(repo, &["update-ref", "-d", &staging]);
    result
}

/// Point `staging` at the current config tip, build the new config commit on it,
/// then push it signed onto `refs/meta/config`.
fn stage_and_push(
    repo: &Path,
    staging: &str,
    config: &git_ents::config::Config,
    private_key: &str,
    username: &str,
    seed: &str,
    hooks: &Path,
) -> Result<(), String> {
    if let Some(tip) = rev_parse(repo, git_ents::config::CONFIG_REF) {
        git(repo, &["update-ref", staging, &tip])
            .map_err(|e| format!("could not stage the edit: {e}"))?;
    }
    git_ents::config::store_to_ref(repo, staging, config)
        .map_err(|e| format!("could not build the edit: {e}"))?;

    let keydir = tempfile::tempdir().map_err(|e| format!("could not create temp dir: {e}"))?;
    let keyfile = keydir.path().join("web-key");
    write_private_key(&keyfile, private_key)?;

    let hooks = hooks
        .to_str()
        .ok_or_else(|| "hooks path is not UTF-8".to_owned())?;
    let receive_pack = format!(
        "git -c receive.certNonceSeed={seed} -c receive.certNonceSlop=60 -c core.hooksPath={hooks} receive-pack"
    );
    let url = format!("file://{}", repo.display());
    let refspec = format!("{staging}:{}", git_ents::config::CONFIG_REF);

    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", "gpg.format=ssh"])
        .arg("-c")
        .arg(format!("user.signingkey={}", keyfile.display()))
        .arg("-c")
        .arg(format!("user.name={username}"))
        .arg("-c")
        .arg(format!("user.email={username}@web"))
        .arg("push")
        .arg("--signed")
        .arg(format!("--receive-pack={receive_pack}"))
        .arg(&url)
        .arg(&refspec)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("could not run git push: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(push_error(&output.stderr))
    }
}

/// The username of the member whose web key matches `public_key`, if any. The
/// match is on the key type and body, ignoring any trailing comment.
pub(super) fn member_for_public_key(repo: &Path, public_key: &str) -> Option<String> {
    let wanted = normalize_key(public_key);
    let members = git_ents::members::load_all(repo).ok()?;
    members.into_iter().find_map(|member| {
        member
            .keys()
            .iter()
            .any(|(_fingerprint, key)| normalize_key(key) == wanted)
            .then(|| member.principal.clone())
    })
}

/// A public key reduced to its type and body, dropping the comment so two lines
/// for the same key compare equal.
fn normalize_key(line: &str) -> String {
    let mut parts = line.split_whitespace();
    let kind = parts.next().unwrap_or_default();
    let body = parts.next().unwrap_or_default();
    format!("{kind} {body}")
}

/// The key's type word, used as a fallback label.
fn key_type(public_key: &str) -> String {
    public_key
        .split_whitespace()
        .next()
        .unwrap_or("key")
        .to_owned()
}

/// Derive the public key line from a PEM private key, validating it parses.
fn derive_public_key(private_key: &str) -> Result<String, String> {
    let dir = tempfile::tempdir().map_err(|e| format!("could not create temp dir: {e}"))?;
    let keyfile = dir.path().join("web-key");
    write_private_key(&keyfile, private_key)?;
    let output = Command::new("ssh-keygen")
        .arg("-y")
        .arg("-f")
        .arg(&keyfile)
        .output()
        .map_err(|e| format!("could not run ssh-keygen: {e}"))?;
    if !output.status.success() {
        return Err("that does not look like a usable private key".to_owned());
    }
    let line = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if line.is_empty() {
        return Err("could not derive a public key".to_owned());
    }
    Ok(line)
}

/// Write a private key to `path` with `0600` permissions and a trailing newline,
/// alongside no public key — ssh derives the public half when signing.
fn write_private_key(path: &Path, private_key: &str) -> Result<(), String> {
    let mut contents = private_key.trim_end().to_owned();
    contents.push('\n');
    std::fs::write(path, &contents).map_err(|e| format!("could not write key: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("could not secure key file: {e}"))?;
    }
    Ok(())
}

/// The pre-receive rejection reason from git's stderr, or a generic message.
fn push_error(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    text.lines()
        .find_map(|line| line.trim().strip_prefix("remote: error: "))
        .or_else(|| {
            text.lines()
                .find_map(|line| line.trim().strip_prefix("remote: "))
                .filter(|l| !l.is_empty())
        })
        .map(str::to_owned)
        .unwrap_or_else(|| "the push was rejected".to_owned())
}

/// The committed tip of `refname`, or `None` when the ref is absent.
fn rev_parse(repo: &Path, refname: &str) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", "--quiet", refname])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    let oid = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (output.status.success() && !oid.is_empty()).then_some(oid)
}

/// Run `git <args>` in `repo`, returning trimmed stdout or an error message.
fn git(repo: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("could not run git: {e}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}

/// The session token in a `Cookie` header value, if present.
fn token(cookie: &str) -> Option<String> {
    cookie.split(';').find_map(|pair| {
        let (name, value) = pair.trim().split_once('=')?;
        (name == COOKIE).then(|| value.to_owned())
    })
}

/// A fresh, unguessable session token: 32 random bytes from the OS, hex-encoded.
fn random_token() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|e| format!("could not read randomness: {e}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// One field's decoded value from an `application/x-www-form-urlencoded` body.
pub(super) fn field(body: &[u8], name: &str) -> Option<String> {
    form(body).remove(name)
}

/// Parse an `application/x-www-form-urlencoded` body into its fields.
fn form(body: &[u8]) -> HashMap<String, String> {
    let text = String::from_utf8_lossy(body);
    text.split('&')
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            Some((percent_decode(key), percent_decode(value)))
        })
        .collect()
}

/// Decode one form field: `+` to space and `%XX` to its byte.
fn percent_decode(input: &str) -> String {
    let spaced = input.replace('+', " ");
    let mut parts = spaced.split('%');
    let mut out: Vec<u8> = parts.next().unwrap_or_default().as_bytes().to_vec();
    for part in parts {
        let bytes = part.as_bytes();
        match (
            bytes.first().copied().and_then(hex_value),
            bytes.get(1).copied().and_then(hex_value),
        ) {
            (Some(hi), Some(lo)) => {
                out.push((hi << 4) | lo);
                out.extend_from_slice(part.get(2..).unwrap_or_default().as_bytes());
            }
            _ => {
                out.push(b'%');
                out.extend_from_slice(bytes);
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A single hex digit's value, `0..=15`.
fn hex_value(byte: u8) -> Option<u8> {
    (byte as char)
        .to_digit(16)
        .and_then(|d| u8::try_from(d).ok())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "unit test")]
    use super::*;

    #[test]
    fn normalizes_keys_by_dropping_the_comment() {
        assert_eq!(
            normalize_key("ssh-ed25519 AAAABODY laptop@host"),
            normalize_key("ssh-ed25519 AAAABODY web"),
        );
    }

    #[test]
    fn decodes_form_fields() {
        assert_eq!(
            field(b"label=my+web+key&private_key=line1%0Aline2", "label").as_deref(),
            Some("my web key"),
        );
        assert_eq!(
            field(b"label=my+web+key&private_key=line1%0Aline2", "private_key").as_deref(),
            Some("line1\nline2"),
        );
    }

    #[test]
    fn reads_the_session_cookie() {
        assert_eq!(
            token("other=1; ents_session=abc123; x=2").as_deref(),
            Some("abc123"),
        );
        assert_eq!(token("other=1").as_deref(), None);
    }

    #[test]
    fn random_tokens_are_long_and_distinct() {
        let a = random_token().unwrap();
        let b = random_token().unwrap();
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
    }
}
