#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::panic,
    clippy::unused_result_ok,
    reason = "integration test binary"
)]

//! The Phase 3 CA-pin gate: a certificate the pinned CA issued verifies against
//! the `allowed_signers` file `git_ents_core::members` renders, and a certificate
//! from an unpinned CA does not. The cert-embedded signature is produced the way
//! a real client would — through an `ssh-agent` holding the key and its
//! certificate — since `ssh-keygen -Y sign` only embeds a certificate when the
//! agent supplies it.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use git_ents_core::members::{Member, allowed_signers};

/// The principal the CA certifies and the verifier checks — the pusher identity.
const PRINCIPAL: &str = "tester@example.com";

#[test]
fn a_cert_from_the_pinned_ca_verifies_and_an_unpinned_one_does_not() {
    let Some(agent) = Agent::start() else {
        // No usable ssh-agent (unusual, but don't fail the suite over the
        // environment); the unit tests still cover the rendered line.
        eprintln!("skipping: could not start ssh-agent");
        return;
    };
    let dir = unique_dir();

    let ca = keygen(&dir, "ca");
    let other_ca = keygen(&dir, "other-ca");
    let user = keygen(&dir, "user");
    certify(&dir, &ca, &user, PRINCIPAL);
    agent.add(&user);

    let message = dir.join("msg");
    std::fs::write(&message, "payload\n").unwrap();
    let signature = agent.sign(&user_cert(&user), &message);

    // The pinned CA's `allowed_signers` accepts the cert it issued.
    let pinned = render_ca_allowed_signers(&dir, "pinned", &ca);
    assert!(
        verify(&pinned, PRINCIPAL, &message, &signature),
        "a cert from the pinned CA was rejected"
    );

    // A different CA's `allowed_signers` rejects it.
    let unpinned = render_ca_allowed_signers(&dir, "unpinned", &other_ca);
    assert!(
        !verify(&unpinned, PRINCIPAL, &message, &signature),
        "a cert from an unpinned CA was accepted"
    );

    agent.stop();
    std::fs::remove_dir_all(&dir).ok();
}

/// Write the `allowed_signers` file `git_ents_core::members` renders for a member
/// whose trust is the CA `ca`, and return its path.
fn render_ca_allowed_signers(dir: &Path, name: &str, ca: &Key) -> PathBuf {
    let ca_pubkey = std::fs::read_to_string(&ca.public)
        .unwrap()
        .trim()
        .to_owned();
    let member = Member::with_ca("anyone".to_owned(), ca_pubkey);
    // Sanity: a CA member exposes no leaf keys.
    assert!(member.keys().is_empty());
    let path = dir.join(name);
    std::fs::write(&path, allowed_signers(&[member])).unwrap();
    path
}

/// An ed25519 key pair: its private and public key paths.
struct Key {
    private: PathBuf,
    public: PathBuf,
}

fn keygen(dir: &Path, name: &str) -> Key {
    let private = dir.join(name);
    let status = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", name, "-f"])
        .arg(&private)
        .status()
        .unwrap();
    assert!(status.success(), "ssh-keygen failed");
    Key {
        public: dir.join(format!("{name}.pub")),
        private,
    }
}

/// The certificate path `ssh-keygen` writes beside a signed public key.
fn user_cert(user: &Key) -> PathBuf {
    user.private.with_file_name(format!(
        "{}-cert.pub",
        user.private.file_name().unwrap().to_str().unwrap()
    ))
}

/// Have `ca` issue a user certificate for `user` valid for `principal`.
fn certify(dir: &Path, ca: &Key, user: &Key, principal: &str) {
    let status = Command::new("ssh-keygen")
        .arg("-q")
        .arg("-s")
        .arg(&ca.private)
        .args(["-I", "test-id", "-n", principal, "-V", "-1d:+365d"])
        .arg(&user.public)
        .current_dir(dir)
        .status()
        .unwrap();
    assert!(
        status.success(),
        "ssh-keygen could not issue the certificate"
    );
}

/// Verify `signature` over `message` against `allowed`, returning whether
/// `ssh-keygen -Y verify` accepts it for `principal`.
fn verify(allowed: &Path, principal: &str, message: &Path, signature: &Path) -> bool {
    use std::io::Write as _;
    let payload = std::fs::read(message).unwrap();
    let mut child = Command::new("ssh-keygen")
        .args(["-Y", "verify", "-n", "git", "-I", principal, "-f"])
        .arg(allowed)
        .arg("-s")
        .arg(signature)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(&payload).unwrap();
    child.wait().unwrap().success()
}

/// A running `ssh-agent` the test signs through.
struct Agent {
    sock: String,
    pid: String,
}

impl Agent {
    /// Start an `ssh-agent`, parsing its socket and pid from the shell snippet it
    /// prints. Returns `None` when no agent could be started.
    fn start() -> Option<Self> {
        let output = Command::new("ssh-agent").arg("-s").output().ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let field = |key: &str| {
            text.split(';')
                .find_map(|part| part.trim().strip_prefix(&format!("{key}=")))
                .map(str::to_owned)
        };
        Some(Self {
            sock: field("SSH_AUTH_SOCK")?,
            pid: field("SSH_AGENT_PID")?,
        })
    }

    /// Load `key` (and the certificate beside it) into the agent.
    fn add(&self, key: &Key) {
        let status = Command::new("ssh-add")
            .arg(&key.private)
            .env("SSH_AUTH_SOCK", &self.sock)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "ssh-add failed");
    }

    /// Sign `message` through the agent using the certificate `cert`, returning
    /// the signature path. Pointing `-f` at the certificate makes the agent embed
    /// it in the SSHSIG, which is what a `cert-authority` line verifies against.
    fn sign(&self, cert: &Path, message: &Path) -> PathBuf {
        let status = Command::new("ssh-keygen")
            .args(["-Y", "sign", "-n", "git", "-f"])
            .arg(cert)
            .arg(message)
            .env("SSH_AUTH_SOCK", &self.sock)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(
            status.success(),
            "ssh-keygen -Y sign through the agent failed"
        );
        message.with_file_name(format!(
            "{}.sig",
            message.file_name().unwrap().to_str().unwrap()
        ))
    }

    fn stop(&self) {
        Command::new("ssh-agent")
            .args(["-k"])
            .env("SSH_AUTH_SOCK", &self.sock)
            .env("SSH_AGENT_PID", &self.pid)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok();
    }
}

fn unique_dir() -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("git-ents-ca-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
