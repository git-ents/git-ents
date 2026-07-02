//! Authenticated browser writes.
//!
//! Signing in never surrenders a private key. The server issues a one-time
//! challenge; the member signs it locally with their web key and pastes back the
//! signature and their public key. The server verifies that signature against
//! the pasted key, which proves the browser controls it — the same proof a CLI
//! push gives, without the key ever leaving the member's machine.
//!
//! An edit is then landed as a real `git push --signed` onto `refs/meta/config`,
//! signed with the *server's own* member key, so it passes the very same
//! `pre-receive` gate a CLI push does. The commit's author is the signed-in
//! human (resolved from their key's membership); the committer is the server.
//! Nothing secret to the member is ever held or persisted: a session keeps only
//! their public key.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The cookie that carries a session token.
pub(super) const COOKIE: &str = "ents_session";

/// The SSHSIG namespace a sign-in signature is made under — distinct from git's
/// own `git` namespace, so a login signature can never double as a push and vice
/// versa.
pub(super) const LOGIN_NAMESPACE: &str = "git.ents.cloud";

/// How long an issued sign-in challenge stays valid.
const CHALLENGE_TTL: Duration = Duration::from_secs(600);

/// In-memory session table, shared by every handler.
pub(crate) type Sessions = Arc<Mutex<HashMap<String, Session>>>;

/// Outstanding sign-in challenges and when each was issued; consumed once.
pub(crate) type Challenges = Arc<Mutex<HashMap<String, Instant>>>;

/// One browser session. It holds only the member's *public* key — enough to
/// authorize per repository — plus a display label and a CSRF token.
pub(crate) struct Session {
    /// The member's public key line (`type base64`), matched against members.
    public_key: String,
    /// A human label for the key — its comment, or its type.
    label: String,
    /// A per-session token that state-changing form posts must echo back, so a
    /// cross-site request (which cannot read it) cannot act as the user.
    csrf: String,
}

/// A cheap, cloneable view of a session for rendering and authorization.
#[derive(Clone)]
pub(super) struct SessionSnapshot {
    pub(super) label: String,
    pub(super) public_key: String,
    pub(super) csrf: String,
}

/// The fields a settings edit may change on `refs/meta/config`.
pub(super) struct ConfigEdit {
    pub(super) description: String,
    pub(super) homepage: String,
    pub(super) topics: Vec<String>,
}

/// Create an empty session table.
pub(crate) fn new_sessions() -> Sessions {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Create an empty challenge table.
pub(crate) fn new_challenges() -> Challenges {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Issue a fresh one-time sign-in challenge, returning the nonce to sign.
pub(super) fn issue_challenge(challenges: &Challenges) -> Result<String, String> {
    let nonce = random_token()?;
    let mut table = challenges
        .lock()
        .map_err(|_poisoned| "challenge store unavailable".to_owned())?;
    let now = Instant::now();
    table.retain(|_nonce, issued| now.duration_since(*issued) < CHALLENGE_TTL);
    table.insert(nonce.clone(), now);
    Ok(nonce)
}

/// Consume `nonce`, returning whether it was a live, unexpired challenge.
fn take_challenge(challenges: &Challenges, nonce: &str) -> bool {
    let Ok(mut table) = challenges.lock() else {
        return false;
    };
    match table.remove(nonce) {
        Some(issued) => Instant::now().duration_since(issued) < CHALLENGE_TTL,
        None => false,
    }
}

/// The session a `Cookie` header points at, as a snapshot, if any.
pub(super) fn snapshot(sessions: &Sessions, cookie: Option<&str>) -> Option<SessionSnapshot> {
    let token = token(cookie?)?;
    let table = sessions.lock().ok()?;
    let session = table.get(&token)?;
    Some(SessionSnapshot {
        label: session.label.clone(),
        public_key: session.public_key.clone(),
        csrf: session.csrf.clone(),
    })
}

/// Complete a sign-in: verify the pasted `signature` over the issued `nonce`
/// against the pasted `public_key`, and on success open a session and return its
/// token. Holding a session grants nothing on its own — an edit is authorized
/// per repository against the live member list.
pub(super) fn login(
    sessions: &Sessions,
    challenges: &Challenges,
    body: &[u8],
) -> Result<String, String> {
    let fields = form(body);
    let public_key = trimmed(&fields, "public_key");
    let signature = trimmed(&fields, "signature");
    let nonce = trimmed(&fields, "nonce");
    if public_key.is_empty() || signature.is_empty() {
        return Err("paste your public key and the signature".to_owned());
    }
    if !take_challenge(challenges, &nonce) {
        return Err("your sign-in challenge expired; reload and try again".to_owned());
    }
    if !verify_login_signature(&public_key, &nonce, &signature)? {
        return Err("the signature did not match that public key".to_owned());
    }

    let label = key_comment(&public_key).unwrap_or_else(|| key_type(&public_key));
    let token = random_token()?;
    let csrf = random_token()?;
    let mut table = sessions
        .lock()
        .map_err(|_poisoned| "session store unavailable".to_owned())?;
    table.insert(
        token.clone(),
        Session {
            public_key: normalize_key(&public_key),
            label,
            csrf,
        },
    );
    Ok(token)
}

/// Whether `cookie`'s session exists and its CSRF token matches `token`.
pub(super) fn csrf_ok(sessions: &Sessions, cookie: Option<&str>, token: &str) -> bool {
    let Some(session_token) = cookie.and_then(self::token) else {
        return false;
    };
    sessions
        .lock()
        .ok()
        .and_then(|table| table.get(&session_token).map(|s| s.csrf == token))
        .unwrap_or(false)
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

/// Refuse `username` unless they are [`Provenance::AdminRegistered`]: a
/// self-attested web member gets limited trust and may not edit settings
/// (outside the allowed set of issues/comments) until an admin promotes them.
/// `pre_receive` cannot enforce this — it is purely key-based, and a
/// self-attested member typically has no push key to gate — so the web write
/// path is the enforcement point.
fn require_admin_registered(store: &git_store::Store, username: &str) -> Result<(), String> {
    use git_ents::members::Provenance;
    let member = git_ents::members::load_with(store, username)
        .map_err(|e| format!("could not read member: {e}"))?
        .ok_or_else(|| "your web key is not a member of this repository".to_owned())?;
    match member.provenance {
        Provenance::AdminRegistered => Ok(()),
        Provenance::SelfAttestedWeb => Err(
            "self-attested members may only edit issues and comments until an admin promotes them"
                .to_owned(),
        ),
    }
}

/// Land a configuration change: stage it on a throwaway ref authored by the
/// signed-in member, then push it onto `refs/meta/config` signed with the
/// server's key, through the `pre-receive` gate. Returns `Ok` only when the gate
/// accepts the push.
///
/// `seed`, `hooks`, and `signing_key` are the server's signed-push nonce seed,
/// hooks directory, and own member key; all are required, so a web edit is never
/// a way around a server that is not enforcing the gate.
pub(super) fn edit_config(
    sessions: &Sessions,
    cookie: Option<&str>,
    repo: &Path,
    edit: &ConfigEdit,
    seed: &str,
    hooks: &Path,
    signing_key: &Path,
) -> Result<(), String> {
    let token = cookie
        .and_then(token)
        .ok_or_else(|| "sign in to edit settings".to_owned())?;
    let public_key = {
        let table = sessions
            .lock()
            .map_err(|_poisoned| "session store unavailable".to_owned())?;
        table
            .get(&token)
            .ok_or_else(|| "sign in to edit settings".to_owned())?
            .public_key
            .clone()
    };

    let store = git_store::Store::open(repo).map_err(|e| format!("cannot open store: {e}"))?;
    let username = member_for_public_key_with(&store, &public_key)
        .ok_or_else(|| "your web key is not a member of this repository".to_owned())?;
    require_admin_registered(&store, &username)?;

    let mut config =
        git_ents::config::load_with(&store).map_err(|e| format!("could not read config: {e}"))?;
    config.description = edit.description.clone();
    config.homepage = edit.homepage.clone();
    config.topics = edit.topics.clone();

    signed_edit(
        repo,
        git_ents::config::CONFIG_REF,
        &config,
        "Update configuration",
        &username,
        signing_key,
        seed,
        hooks,
    )
}

/// Land `value` onto `target_ref` as a real `git push --signed`, authored by
/// `username` and signed with the server's own `signing_key`, through the
/// same `pre-receive` gate a CLI push traverses — the one landing operation
/// every authenticated browser write shares, whatever meta-ref it targets.
///
/// The contract: `value` is built on a fresh ref staged at `target_ref`'s
/// current tip (so the push is a clean fast-forward), the staging ref is
/// *always* deleted before returning — whether or not the push was accepted,
/// so a rejected edit never leaves a zombie ref behind — and the commit that
/// lands is authored by `username` while the server is the committer.
#[expect(
    clippy::too_many_arguments,
    reason = "the server identity a signed edit requires"
)]
fn signed_edit<T: for<'a> facet::Facet<'a>>(
    repo: &Path,
    target_ref: &str,
    value: &T,
    message: &str,
    username: &str,
    signing_key: &Path,
    seed: &str,
    hooks: &Path,
) -> Result<(), String> {
    let staging = format!("refs/web-staging/{}", random_token()?);
    let result = stage_and_push(
        repo,
        &staging,
        target_ref,
        value,
        message,
        username,
        signing_key,
        seed,
        hooks,
    );
    // Clean up the staging ref whether or not the push was accepted.
    let _cleanup = git(repo, &["update-ref", "-d", &staging]);
    result
}

/// Point `staging` at `target_ref`'s current tip, build the new commit on it
/// authored by `username`, then push it signed with the server's key onto
/// `target_ref`.
#[expect(clippy::too_many_arguments, reason = "internal step of signed_edit")]
fn stage_and_push<T: for<'a> facet::Facet<'a>>(
    repo: &Path,
    staging: &str,
    target_ref: &str,
    value: &T,
    message: &str,
    username: &str,
    signing_key: &Path,
    seed: &str,
    hooks: &Path,
) -> Result<(), String> {
    if let Some(tip) = rev_parse(repo, target_ref) {
        git(repo, &["update-ref", staging, &tip])
            .map_err(|e| format!("could not stage the edit: {e}"))?;
    }
    let email = format!("{username}@web");
    git_store::Store::open(repo)
        .map_err(|e| format!("could not open store: {e}"))?
        .store_authored(staging, value, message, (username, &email))
        .map_err(|e| format!("could not build the edit: {e}"))?;

    let signer = signing_key
        .to_str()
        .ok_or_else(|| "signing key path is not UTF-8".to_owned())?;
    let hooks = hooks
        .to_str()
        .ok_or_else(|| "hooks path is not UTF-8".to_owned())?;
    let receive_pack = format!(
        "git -c receive.certNonceSeed={seed} -c receive.certNonceSlop=60 -c core.hooksPath={hooks} receive-pack"
    );
    let url = format!("file://{}", repo.display());
    let refspec = format!("{staging}:{target_ref}");

    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", "gpg.format=ssh"])
        .arg("-c")
        .arg(format!("user.signingkey={signer}"))
        .args([
            "-c",
            "user.name=git-ents-web",
            "-c",
            "user.email=web@git-ents",
        ])
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

/// Verify an SSHSIG `signature` over `nonce` was made by `public_key` under the
/// login namespace, using `ssh-keygen -Y verify` against a one-key allowed
/// signers file.
fn verify_login_signature(public_key: &str, nonce: &str, signature: &str) -> Result<bool, String> {
    let dir = tempfile::tempdir().map_err(|e| format!("could not create temp dir: {e}"))?;
    let allowed = dir.path().join("allowed_signers");
    let sig = dir.path().join("nonce.sig");
    write_file(
        &allowed,
        format!(
            "* namespaces=\"{LOGIN_NAMESPACE}\" {}\n",
            normalize_key(public_key)
        )
        .as_bytes(),
    )?;
    write_file(&sig, signature.as_bytes())?;

    let mut child = Command::new("ssh-keygen")
        .args(["-Y", "verify", "-n", LOGIN_NAMESPACE, "-I", "web", "-f"])
        .arg(&allowed)
        .arg("-s")
        .arg(&sig)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not run ssh-keygen: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(nonce.as_bytes())
            .map_err(|e| format!("could not hand the challenge to ssh-keygen: {e}"))?;
    }
    Ok(child
        .wait()
        .map_err(|e| format!("ssh-keygen did not complete: {e}"))?
        .success())
}

/// The username of the member whose web key matches `public_key`, if any, from
/// an already-open `store`. The match is on the key type and body, ignoring
/// any trailing comment.
///
/// O(m×k): loads every member and scans each one's keys. Acceptable at
/// current scale; a batch path exists on the other axis
/// (`members::load_all_indexed`, principal → member) but this lookup goes the
/// other way (key → member), which would need its own index — deferred until
/// measured, since a member legitimately holds more than one key, ruling out
/// a simple bi-map.
pub(super) fn member_for_public_key_with(
    store: &git_store::Store,
    public_key: &str,
) -> Option<String> {
    let wanted = normalize_key(public_key);
    let members = git_ents::members::load_all_with(store).ok()?;
    members.into_iter().find_map(|member| {
        member
            .keys()
            .iter()
            .any(|(_fingerprint, key)| normalize_key(key) == wanted)
            .then(|| member.principal.clone())
    })
}

/// The username of the member whose web key matches `public_key`, if any. See
/// [`member_for_public_key_with`].
pub(super) fn member_for_public_key(repo: &Path, public_key: &str) -> Option<String> {
    member_for_public_key_with(&git_store::Store::open(repo).ok()?, public_key)
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

/// The key's trailing comment, if it carries one.
fn key_comment(public_key: &str) -> Option<String> {
    public_key
        .split_whitespace()
        .nth(2)
        .map(str::to_owned)
        .filter(|comment| !comment.is_empty())
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    std::fs::write(path, bytes).map_err(|e| format!("could not write {}: {e}", path.display()))
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

/// A fresh, unguessable token: 32 random bytes from the OS, hex-encoded.
fn random_token() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| format!("could not read randomness: {e}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// One field's decoded value from an `application/x-www-form-urlencoded` body.
pub(super) fn field(body: &[u8], name: &str) -> Option<String> {
    form(body).remove(name)
}

/// A form field's trimmed value, or the empty string.
fn trimmed(fields: &HashMap<String, String>, name: &str) -> String {
    fields
        .get(name)
        .map(|v| v.trim())
        .unwrap_or_default()
        .to_owned()
}

/// Parse an `application/x-www-form-urlencoded` body into its fields.
fn form(body: &[u8]) -> HashMap<String, String> {
    form_urlencoded::parse(body).into_owned().collect()
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
    fn reads_a_keys_comment_as_its_label() {
        assert_eq!(
            key_comment("ssh-ed25519 AAAA laptop").as_deref(),
            Some("laptop")
        );
        assert_eq!(key_comment("ssh-ed25519 AAAA"), None);
    }

    #[test]
    fn decodes_form_fields() {
        assert_eq!(
            field(b"public_key=ssh-ed25519+AAAA&signature=a%0Ab", "public_key").as_deref(),
            Some("ssh-ed25519 AAAA"),
        );
        assert_eq!(
            field(b"public_key=ssh-ed25519+AAAA&signature=a%0Ab", "signature").as_deref(),
            Some("a\nb"),
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
    fn a_consumed_challenge_does_not_verify_twice() {
        let challenges = new_challenges();
        let nonce = issue_challenge(&challenges).unwrap();
        assert!(take_challenge(&challenges, &nonce), "first use should pass");
        assert!(
            !take_challenge(&challenges, &nonce),
            "a challenge is one-time"
        );
    }
}
