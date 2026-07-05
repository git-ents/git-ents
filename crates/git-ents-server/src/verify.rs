//! The `pre-receive` verifier: a git hook that gates pushes on a signature from
//! a member.
//!
//! When no member is listed under `refs/meta/member/*` the server is still in
//! its open bootstrap window and every push is allowed, so the first member can
//! be pushed in. Once any member is listed, a push must carry a signed-push
//! certificate (`git push --signed`) whose anti-replay nonce git accepted and
//! whose signature verifies against one of those members' in-window keys —
//! minus any fingerprint on the `refs/meta/revoked` deny list, which is
//! subtracted from the trust set before the check so a revoked key fails the
//! moment it is listed, faster than its window would expire.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use git_ents_core::config;
use git_member::members::{self, Member};
use git_member::revocations;

/// Verify the push git is about to apply, returning `Ok(())` to accept it or
/// `Err(reason)` to reject it. The push certificate is read from the
/// environment git populates for the hook.
///
/// ## Requirements
///
/// @relation(auth.signed-push, compat.openssh-signed-push)
pub fn pre_receive() -> Result<(), String> {
    let repo = std::env::current_dir().map_err(|e| format!("cannot resolve repository: {e}"))?;
    let store = git_store::Store::open(&repo).map_err(|e| format!("cannot open store: {e}"))?;
    let members = members::load_all_with(&store)
        .map_err(|e| format!("could not read authorized signers: {e}"))?;
    // @relation(auth.bootstrap)
    if members.is_empty() {
        // No trust list pushed yet: stay open so the first signer can be added.
        // Revocation is keyed on member refs existing, so revoking every member's
        // keys leaves the set empty and fails closed rather than reopening this
        // bootstrap window.
        return Ok(());
    }
    let revoked = revocations::fingerprints_with(&store)
        .map_err(|e| format!("could not read revocations: {e}"))?;
    // @relation(auth.signed-push)
    let authorized = members::without_revoked(members, &revoked);
    let ref_updates = read_ref_updates()?;

    let cert_oid = env("GIT_PUSH_CERT")
        .filter(|oid| !oid.is_empty())
        .ok_or_else(|| {
            "this repository requires a signed push: rerun with `git push --signed`".to_owned()
        })?;
    // @relation(auth.signed-push)
    if env("GIT_PUSH_CERT_NONCE_STATUS").as_deref() != Some("OK") {
        return Err("push certificate nonce was missing or stale".to_owned());
    }

    let certificate = cat_blob(&repo, &cert_oid)?;
    verify_certificate(&authorized, &certificate)?;

    let signer = identify_signer(&authorized, &certificate);
    if let Some(member) = signer {
        let config =
            config::load_with(&store).map_err(|e| format!("could not read configuration: {e}"))?;
        for ref_name in &ref_updates {
            if !git_member::ref_allowed(&config, member.role.as_deref(), ref_name) {
                return Err(format!(
                    "{} (role {:?}) is not permitted to push to {ref_name:?}",
                    member.principal, member.role
                ));
            }
        }
    }
    Ok(())
}

/// The ref names git is about to update, read from the hook's own stdin
/// (`<old-oid> <new-oid> <refname>` per line) — distinct from the certificate
/// payload, which is written to a separate `ssh-keygen` child process below.
fn read_ref_updates() -> Result<Vec<String>, String> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .map_err(|e| format!("could not read ref updates: {e}"))?;
    Ok(input
        .lines()
        .filter_map(|line| line.split_whitespace().nth(2))
        .map(str::to_owned)
        .collect())
}

/// Which of `authorized` signed `certificate`, by re-checking the signature
/// against each member's own key set individually. `verify_certificate`
/// already established the signature matches *someone* in `authorized`; this
/// narrows it to a specific member so their `role` can gate the ref update.
fn identify_signer<'a>(authorized: &'a [Member], certificate: &str) -> Option<&'a Member> {
    authorized
        .iter()
        .find(|member| verify_certificate(std::slice::from_ref(member), certificate).is_ok())
}

/// Split the certificate into its signed payload and SSH signature, then accept
/// it only when `ssh-keygen -Y verify` trusts the signature against one of the
/// authorized keys.
///
/// ## Requirements
///
/// @relation(auth.signed-push, compat.ssh-keygen)
fn verify_certificate(authorized: &[Member], certificate: &str) -> Result<(), String> {
    const MARKER: &str = "-----BEGIN SSH SIGNATURE-----";
    let split = certificate
        .find(MARKER)
        .ok_or_else(|| "push certificate carries no SSH signature".to_owned())?;
    let (payload, signature) = certificate.split_at(split);
    let principal = signer_principal(certificate);

    let workdir = tempfile::tempdir().map_err(|e| format!("could not create temp dir: {e}"))?;
    let allowed_path = workdir.path().join("allowed_signers");
    let signature_path = workdir.path().join("cert.sig");
    write_file(
        &allowed_path,
        members::allowed_signers(authorized).as_bytes(),
    )?;
    write_file(&signature_path, signature.as_bytes())?;

    let mut child = Command::new("ssh-keygen")
        .args(["-Y", "verify", "-n", "git", "-I", principal, "-f"])
        .arg(&allowed_path)
        .arg("-s")
        .arg(&signature_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not run ssh-keygen: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(payload.as_bytes())
            .map_err(|e| format!("could not hand the certificate to ssh-keygen: {e}"))?;
    }
    let status = child
        .wait()
        .map_err(|e| format!("ssh-keygen did not complete: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("push is not signed by an authorized key".to_owned())
    }
}

/// The pusher's email from the certificate's `pusher` line, used as the
/// `ssh-keygen` principal. The authorized set uses a wildcard principal, so any
/// non-empty identity matches; `git` is a harmless fallback.
fn signer_principal(certificate: &str) -> &str {
    certificate
        .lines()
        .find_map(|line| line.strip_prefix("pusher "))
        .and_then(|rest| rest.split_once('<'))
        .and_then(|(_, rest)| rest.split_once('>'))
        .map(|(email, _)| email)
        .unwrap_or("git")
}

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

// @relation(compat.git)
/// Read the blob `oid` from `repo` as text.
fn cat_blob(repo: &Path, oid: &str) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["cat-file", "blob", oid])
        .output()
        .map_err(|e| format!("could not read push certificate: {e}"))?;
    if !output.status.success() {
        return Err("could not read the push certificate from the object store".to_owned());
    }
    String::from_utf8(output.stdout)
        .map_err(|_invalid| "push certificate is not valid UTF-8".to_owned())
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    std::fs::write(path, bytes).map_err(|e| format!("could not write {}: {e}", path.display()))
}
