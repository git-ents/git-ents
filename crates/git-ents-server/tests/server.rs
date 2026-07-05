#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    reason = "integration test binary"
)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::Command;

use rstest::rstest;

// r[verify web.server-rendered] - GET / returns a rendered page, no client required
// r[verify web.index] - GET / renders the repository index
// r[verify server.embeddable] - exercises the standalone `git-ents-server` binary
#[test]
fn responds_to_requests() {
    let port = free_port();

    let mut child = Command::new(env!("CARGO_BIN_EXE_git-ents-server"))
        .arg("--port")
        .arg(port.to_string())
        .spawn()
        .unwrap();

    wait_for_port(port);

    for i in 0..3 {
        let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
            .unwrap_or_else(|e| panic!("could not connect on request {i}: {e}"));
        stream.write_all(b"GET / HTTP/1.0\r\n\r\n").unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(
            response.contains("200 OK"),
            "unexpected response: {response}"
        );
    }

    child.kill().unwrap();
    let _wait = child.wait();
}

// r[verify storage.bare] - a first push auto-creates the bare repo, and it survives to be cloned
// r[verify namespace.auto-create]
// r[verify protocol.git]
#[test]
fn push_then_clone_round_trip() {
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

    let url = format!("http://127.0.0.1:{port}/test.git");

    // Build a source repo with one commit on `main` and push it (auto-init).
    let src = tempfile::tempdir().unwrap();
    run_git(Some(src.path()), &["init", "-q", "-b", "main"]);
    std::fs::write(src.path().join("README.md"), "hello ents\n").unwrap();
    run_git(Some(src.path()), &["add", "."]);
    run_git(Some(src.path()), &["commit", "-q", "-m", "initial"]);
    run_git(Some(src.path()), &["push", "-q", &url, "main"]);
    let pushed = rev_parse(src.path());

    // Clone it back and confirm the objects round-trip.
    let dst = tempfile::tempdir().unwrap();
    let clone_path = dst.path().join("clone");
    run_git(None, &["clone", "-q", &url, clone_path.to_str().unwrap()]);
    let cloned = rev_parse(&clone_path);

    child.kill().unwrap();
    let _wait = child.wait();

    assert_eq!(pushed, cloned, "cloned HEAD must match pushed HEAD");
}

// r[verify namespace.path] - `org/repo` and `org/team/repo` are accepted multi-segment paths
// r[verify namespace.auto-create]
#[rstest]
#[case("org/repo")]
#[case("org/team/repo")]
fn nested_push_then_clone_round_trip(#[case] name: &str) {
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

    let url = format!("http://127.0.0.1:{port}/{name}.git");

    let src = tempfile::tempdir().unwrap();
    run_git(Some(src.path()), &["init", "-q", "-b", "main"]);
    std::fs::write(src.path().join("README.md"), "hello ents\n").unwrap();
    run_git(Some(src.path()), &["add", "."]);
    run_git(Some(src.path()), &["commit", "-q", "-m", "initial"]);
    run_git(Some(src.path()), &["push", "-q", &url, "main"]);
    let pushed = rev_parse(src.path());

    let dst = tempfile::tempdir().unwrap();
    let clone_path = dst.path().join("clone");
    run_git(None, &["clone", "-q", &url, clone_path.to_str().unwrap()]);
    let cloned = rev_parse(&clone_path);

    child.kill().unwrap();
    let _wait = child.wait();

    assert_eq!(
        pushed, cloned,
        "cloned HEAD must match pushed HEAD for {name}"
    );
}

// r[verify namespace.auto-create] - a colliding creation is refused, not raced
#[rstest]
#[case("org/repo.git/deep.git")] // nested inside an existing repository
#[case("org")] // already exists as a namespace
fn rejects_colliding_pushes(#[case] collide: &str) {
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

    // Claim `org/repo.git`, which also makes `org` a namespace directory.
    let base = format!("http://127.0.0.1:{port}/org/repo.git");
    run_git(Some(src.path()), &["push", "-q", &base, "main"]);

    // A push that collides with that repository must be refused.
    let url = format!("http://127.0.0.1:{port}/{collide}");
    let rejected = !git_command(Some(src.path()), &["push", "-q", &url, "main"])
        .output()
        .unwrap()
        .status
        .success();

    child.kill().unwrap();
    let _wait = child.wait();

    assert!(
        rejected,
        "push colliding with an existing repo ({collide}) must fail"
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
    let output = git_command(Some(dir), &["rev-parse", "HEAD"])
        .output()
        .unwrap();
    assert!(output.status.success());
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
