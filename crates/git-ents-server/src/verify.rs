//! The `pre-receive` verifier: a git hook that gates pushes on a signature from
//! an authorized signer.
//!
//! When the trust list at `refs/meta/auth` is empty the server is still in its
//! open bootstrap window and every push is allowed, so the first signer can be
//! pushed in. Once any signer is listed, a push must carry a signed-push
//! certificate (`git push --signed`) whose anti-replay nonce git accepted and
//! whose signature verifies against one of those keys.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use git_ents::signers::{self, Signer};

/// Verify the push git is about to apply, returning `Ok(())` to accept it or
/// `Err(reason)` to reject it. The push certificate is read from the
/// environment git populates for the hook.
pub fn pre_receive() -> Result<(), String> {
    let repo = std::env::current_dir().map_err(|e| format!("cannot resolve repository: {e}"))?;
    let authorized =
        signers::load(&repo).map_err(|e| format!("could not read authorized signers: {e}"))?;
    if authorized.is_empty() {
        // No trust list pushed yet: stay open so the first signer can be added.
        return Ok(());
    }

    let cert_oid = env("GIT_PUSH_CERT")
        .filter(|oid| !oid.is_empty())
        .ok_or_else(|| {
            "this repository requires a signed push: rerun with `git push --signed`".to_owned()
        })?;
    if env("GIT_PUSH_CERT_NONCE_STATUS").as_deref() != Some("OK") {
        return Err("push certificate nonce was missing or stale".to_owned());
    }

    let certificate = cat_blob(&repo, &cert_oid)?;
    verify_certificate(&authorized, &certificate)
}

/// Split the certificate into its signed payload and SSH signature, then accept
/// it only when `ssh-keygen -Y verify` trusts the signature against one of the
/// authorized keys.
fn verify_certificate(authorized: &[Signer], certificate: &str) -> Result<(), String> {
    const MARKER: &str = "-----BEGIN SSH SIGNATURE-----";
    let split = certificate
        .find(MARKER)
        .ok_or_else(|| "push certificate carries no SSH signature".to_owned())?;
    let (payload, signature) = certificate.split_at(split);
    let principal = signer_principal(certificate);

    let workdir = TempDir::new()?;
    let allowed_path = workdir.path().join("allowed_signers");
    let signature_path = workdir.path().join("cert.sig");
    write_file(
        &allowed_path,
        signers::allowed_signers(authorized).as_bytes(),
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

/// A uniquely named temporary directory removed when dropped, holding the short
/// files `ssh-keygen` needs to read from disk.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Result<Self, String> {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("git-ents-verify-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).map_err(|e| format!("could not create temp dir: {e}"))?;
        Ok(Self(dir))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        match std::fs::remove_dir_all(&self.0) {
            Ok(()) | Err(_) => {}
        }
    }
}
