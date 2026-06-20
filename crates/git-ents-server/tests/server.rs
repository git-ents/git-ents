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

#[test]
fn responds_and_shuts_down() {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);

    let mut child = Command::new(env!("CARGO_BIN_EXE_git-ents-server"))
        .arg("--port")
        .arg(port.to_string())
        .arg("--max-requests")
        .arg("3")
        .spawn()
        .unwrap();

    for i in 0..3 {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut stream = loop {
            match TcpStream::connect(format!("127.0.0.1:{port}")) {
                Ok(s) => break s,
                Err(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(e) => panic!("could not connect on request {i}: {e}"),
            }
        };
        stream.write_all(b"GET / HTTP/1.0\r\n\r\n").unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(
            response.contains("200 OK"),
            "unexpected response: {response}"
        );
    }

    let status = child.wait().unwrap();
    assert!(status.success());
}

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
    run_git(
        None,
        &["clone", "-q", &url, clone_path.to_str().unwrap()],
    );
    let cloned = rev_parse(&clone_path);

    child.kill().unwrap();
    let _wait = child.wait();

    assert_eq!(pushed, cloned, "cloned HEAD must match pushed HEAD");
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
    let output = git_command(Some(dir), &["rev-parse", "HEAD"]).output().unwrap();
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
