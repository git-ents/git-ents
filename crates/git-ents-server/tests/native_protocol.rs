#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    reason = "integration test binary"
)]

//! End-to-end coverage for the native `git-protocol` smart-HTTP path
//! (`crate::native_git`), mounted under `/_native/`. `clone`/`fetch` is
//! proven against a stock `git` binary with zero client configuration, per
//! WS3's read-path interop requirement. The write path is exercised too
//! (a real `git push` during the bootstrap window, since the native
//! endpoint does not yet parse a push certificate off the wire — see
//! `native_git`'s module doc comment); attested-push acceptance/rejection
//! is covered at the trait level in `crates/git-protocol`'s own tests,
//! which construct a `PushRequest` directly with a real signed certificate,
//! per this workstream's test plan.

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
