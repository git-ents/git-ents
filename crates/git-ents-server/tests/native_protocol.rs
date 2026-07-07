#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    reason = "integration test binary"
)]

//! End-to-end coverage for the native `git-protocol` smart-HTTP path
//! (`crate::native_git`), mounted under `/_native/`, all against a stock
//! `git` binary. `clone`/`fetch` is proven with zero client configuration,
//! per WS3's read-path interop requirement. The write path is proven at
//! every attestation stage: a bootstrap-window push (no members enrolled),
//! an unsigned push rejected once a member is enrolled, and a real
//! `git push --signed` (SSH key, the `push.gpgSign` mechanics) accepted
//! with its op record — client certificate embedded by OID — chained under
//! `refs/meta/ops/log`.

use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::Command;

// @relation(protocol.git, compat.git, role=Verifies)
#[test]
fn clones_over_the_native_endpoint_after_a_push_over_the_cgi_endpoint() {
    let data = tempfile::tempdir().unwrap();
    let port = free_port();

    let mut child = Command::new(env!("CARGO_BIN_EXE_git-ents-server"))
        .arg("--port")
        .arg(port.to_string())
        .arg("--data-dir")
        .arg(data.path())
        .spawn()
        .unwrap();
    wait_for_port(port);

    let src = tempfile::tempdir().unwrap();
    run_git(Some(src.path()), &["init", "-q", "-b", "main"]);
    std::fs::write(src.path().join("README.md"), "hello ents\n").unwrap();
    run_git(Some(src.path()), &["add", "."]);
    run_git(Some(src.path()), &["commit", "-q", "-m", "initial"]);
    let pushed = rev_parse(src.path());
    run_git(
        Some(src.path()),
        &[
            "push",
            "-q",
            &format!("http://127.0.0.1:{port}/test.git"),
            "main",
        ],
    );

    let dst = tempfile::tempdir().unwrap();
    let clone_path = dst.path().join("clone");
    run_git(
        None,
        &[
            "clone",
            "-q",
            &format!("http://127.0.0.1:{port}/_native/test.git"),
            clone_path.to_str().unwrap(),
        ],
    );
    let cloned = rev_parse(&clone_path);
    let content = std::fs::read_to_string(clone_path.join("README.md")).unwrap();

    child.kill().unwrap();
    let _wait = child.wait();

    assert_eq!(pushed, cloned, "cloned HEAD must match the pushed HEAD");
    assert_eq!(content, "hello ents\n");
}

// @relation(protocol.git, auth.signed-push, role=Verifies)
#[test]
fn native_push_during_bootstrap_lands_and_emits_an_op_record() {
    let data = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let server_key = scratch.path().join("op-signing-key");
    let status = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&server_key)
        .status()
        .unwrap();
    assert!(status.success(), "ssh-keygen failed");
    let port = free_port();

    let mut child = Command::new(env!("CARGO_BIN_EXE_git-ents-server"))
        .arg("--port")
        .arg(port.to_string())
        .arg("--data-dir")
        .arg(data.path())
        .arg("--web-signing-key")
        .arg(&server_key)
        .spawn()
        .unwrap();
    wait_for_port(port);

    let src = tempfile::tempdir().unwrap();
    run_git(Some(src.path()), &["init", "-q", "-b", "main"]);
    std::fs::write(src.path().join("README.md"), "hello ents\n").unwrap();
    run_git(Some(src.path()), &["add", "."]);
    run_git(Some(src.path()), &["commit", "-q", "-m", "initial"]);
    let pushed = rev_parse(src.path());

    run_git(
        Some(src.path()),
        &[
            "push",
            "-q",
            &format!("http://127.0.0.1:{port}/_native/pushed.git"),
            "main",
        ],
    );

    let repo_on_disk = data.path().join("pushed.git");
    let op_log = rev_parse_ref(&repo_on_disk, "refs/meta/ops/log");
    let landed = rev_parse_ref(&repo_on_disk, "refs/heads/main");
    let op_record = git_command(
        Some(&repo_on_disk),
        &["cat-file", "-p", "refs/meta/ops/log"],
    )
    .output()
    .unwrap();

    child.kill().unwrap();
    let _wait = child.wait();

    assert_eq!(landed, pushed);
    assert!(
        !op_log.is_empty(),
        "op record ref must exist after an accepted push"
    );
    let op_record_text = String::from_utf8_lossy(&op_record.stdout);
    assert!(
        op_record_text.contains("push-cert"),
        "op record must embed the push certificate by OID: {op_record_text}"
    );
    assert!(
        op_record_text.contains(&format!("refs/heads/main {} {pushed}", "0".repeat(40))),
        "op record must record the applied ref edit: {op_record_text}"
    );
}

// @relation(protocol.git, auth.signed-push, role=Verifies)
#[test]
fn signed_push_is_accepted_and_unsigned_rejected_once_a_member_is_enrolled() {
    let data = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let server_key = keygen(scratch.path(), "op-signing-key");
    let member_key = keygen(scratch.path(), "member-key");
    let member_pub = std::fs::read_to_string(scratch.path().join("member-key.pub")).unwrap();
    let port = free_port();

    let mut child = Command::new(env!("CARGO_BIN_EXE_git-ents-server"))
        .arg("--port")
        .arg(port.to_string())
        .arg("--data-dir")
        .arg(data.path())
        .arg("--web-signing-key")
        .arg(&server_key)
        .spawn()
        .unwrap();
    wait_for_port(port);
    let url = format!("http://127.0.0.1:{port}/_native/attested.git");

    // Bootstrap: first push with no members enrolled creates the repo.
    let src = tempfile::tempdir().unwrap();
    run_git(Some(src.path()), &["init", "-q", "-b", "main"]);
    std::fs::write(src.path().join("README.md"), "hello ents\n").unwrap();
    run_git(Some(src.path()), &["add", "."]);
    run_git(Some(src.path()), &["commit", "-q", "-m", "initial"]);
    run_git(Some(src.path()), &["push", "-q", &url, "main"]);

    // Enroll the member key directly on the served bare repo — the same
    // refs/meta/member layout pre-receive trusts.
    let repo_on_disk = data.path().join("attested.git");
    git_member::members::store(
        &repo_on_disk,
        &git_member::members::Member {
            principal: "alice".to_owned(),
            valid_after: None,
            valid_before: None,
            trust: git_member::members::Trust::Keys(
                std::iter::once(("fp1".to_owned(), member_pub)).collect(),
            ),
            provenance: git_member::members::Provenance::AdminRegistered,
            account: None,
            role: None,
        },
    )
    .unwrap();
    let op_log_before = rev_parse_ref(&repo_on_disk, "refs/meta/ops/log");
    assert!(
        !op_log_before.is_empty(),
        "bootstrap push must have logged an op record"
    );

    // A second commit: unsigned push must now be rejected...
    std::fs::write(src.path().join("README.md"), "hello again\n").unwrap();
    run_git(Some(src.path()), &["add", "."]);
    run_git(Some(src.path()), &["commit", "-q", "-m", "second"]);
    let second = rev_parse(src.path());
    let unsigned = git_command(Some(src.path()), &["push", "-q", &url, "main"])
        .output()
        .unwrap();
    assert!(
        !unsigned.status.success(),
        "unsigned push must be rejected once a member is enrolled: {}",
        String::from_utf8_lossy(&unsigned.stderr)
    );

    // ...and the same push signed with the enrolled key must land.
    run_git(
        Some(src.path()),
        &[
            "-c",
            "gpg.format=ssh",
            "-c",
            &format!("user.signingKey={}", member_key.display()),
            "push",
            "-q",
            "--signed",
            &url,
            "main",
        ],
    );

    let landed = rev_parse_ref(&repo_on_disk, "refs/heads/main");
    let op_log_after = rev_parse_ref(&repo_on_disk, "refs/meta/ops/log");
    let op_record = git_command(
        Some(&repo_on_disk),
        &["cat-file", "-p", "refs/meta/ops/log"],
    )
    .output()
    .unwrap();
    let op_record_text = String::from_utf8_lossy(&op_record.stdout).into_owned();
    let cert_oid = op_record_text
        .lines()
        .find_map(|line| line.strip_prefix("push-cert "))
        .unwrap_or_default()
        .to_owned();
    let cert = git_command(Some(&repo_on_disk), &["cat-file", "blob", &cert_oid])
        .output()
        .unwrap();

    child.kill().unwrap();
    let _wait = child.wait();

    assert_eq!(landed, second, "the signed push must have landed");
    assert_ne!(
        op_log_after, op_log_before,
        "the signed push must have chained a new op record"
    );
    let cert_text = String::from_utf8_lossy(&cert.stdout);
    assert!(
        cert_text.contains("BEGIN SSH SIGNATURE"),
        "the embedded push certificate must carry the client's signature: {cert_text}"
    );
    assert!(
        cert_text.contains(&second),
        "the embedded push certificate must name the pushed commit: {cert_text}"
    );
}

/// Generate an ed25519 keypair at `base/<name>`, returning the private key
/// path.
fn keygen(base: &Path, name: &str) -> std::path::PathBuf {
    let key = base.join(name);
    let status = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&key)
        .status()
        .unwrap();
    assert!(status.success(), "ssh-keygen failed");
    key
}

fn free_port() -> u16 {
    let probe = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
    port
}

fn wait_for_port(port: u16) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match TcpStream::connect(format!("127.0.0.1:{port}")) {
            Ok(_) => return,
            Err(_) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(e) => panic!("server never accepted connections: {e}"),
        }
    }
}

fn run_git(dir: Option<&Path>, args: &[&str]) {
    let output = git_command(dir, args).output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn rev_parse(dir: &Path) -> String {
    rev_parse_ref(dir, "HEAD")
}

fn rev_parse_ref(dir: &Path, refname: &str) -> String {
    let output = git_command(Some(dir), &["rev-parse", "--verify", "--quiet", refname])
        .output()
        .unwrap();
    if !output.status.success() {
        return String::new();
    }
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn git_command(dir: Option<&Path>, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0");
    if let Some(dir) = dir {
        cmd.arg("-C").arg(dir);
    }
    cmd.args([
        "-c",
        "user.name=Ent Test",
        "-c",
        "user.email=ent@example.com",
        "-c",
        "commit.gpgsign=false",
    ]);
    cmd.args(args);
    cmd
}
