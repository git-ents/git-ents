#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::unused_result_ok,
    reason = "integration test binary"
)]

//! End-to-end coverage for the `pre-receive` signed-push verifier: a real
//! `git push --signed` over the `file://` transport against a bare repo whose
//! hook is the compiled `git-ents-server pre-receive` subcommand.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

const BIN: &str = env!("CARGO_BIN_EXE_git-ents-server");

/// Run a command and assert it succeeds, returning trimmed stdout.
fn ok(dir: &Path, program: &str, args: &[&str]) -> String {
    let output = git_env(dir, program, args)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{program} {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// Build a command in `dir` with a fixed committer/pusher identity.
fn git_env(dir: &Path, program: &str, args: &[&str]) -> Command {
    let mut command = Command::new(program);
    command
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Tester")
        .env("GIT_AUTHOR_EMAIL", "tester@example.com")
        .env("GIT_COMMITTER_NAME", "Tester")
        .env("GIT_COMMITTER_EMAIL", "tester@example.com");
    command
}

fn unique_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir =
        std::env::temp_dir().join(format!("git-ents-prerecv-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Generate an ed25519 keypair at `base/<name>`, returning the public key path.
fn keygen(base: &Path, name: &str) -> PathBuf {
    let key = base.join(name);
    let status = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", name, "-f"])
        .arg(&key)
        .status()
        .unwrap();
    assert!(status.success(), "ssh-keygen failed");
    base.join(format!("{name}.pub"))
}

/// Create a bare server repo wired to the `pre-receive` verifier, listing the
/// public keys at `authorized` as signers with no validity window.
fn server_repo(base: &Path, authorized: &[&Path]) -> PathBuf {
    let members: Vec<Member> = authorized
        .iter()
        .map(|pubkey| Member {
            pubkey,
            valid_after: None,
            valid_before: None,
        })
        .collect();
    server_repo_with(base, &members)
}

/// One authorized member written into the test `refs/meta/members` doc: a public
/// key and the validity window it is trusted within.
struct Member<'a> {
    pubkey: &'a Path,
    valid_after: Option<&'a str>,
    valid_before: Option<&'a str>,
}

/// Create a bare server repo wired to the `pre-receive` verifier with one
/// `refs/meta/member/member-<n>` ref per member in the real on-disk layout — a
/// `principal` blob, `valid_after`/`valid_before` `Option` subtrees, and a
/// `trust/Keys/key` blob.
fn server_repo_with(base: &Path, members: &[Member]) -> PathBuf {
    let repo = base.join("srv.git");
    ok(
        base,
        "git",
        &["init", "--bare", "-q", repo.to_str().unwrap()],
    );
    ok(
        &repo,
        "git",
        &["config", "receive.certNonceSeed", "test-seed"],
    );

    let hook = repo.join("hooks").join("pre-receive");
    std::fs::write(&hook, format!("#!/bin/sh\nexec \"{BIN}\" pre-receive\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let option_tree = |bound: Option<&str>| match bound {
        None => mktree(&repo, ""),
        Some(value) => {
            let blob = hash_object(&repo, value.as_bytes());
            mktree(&repo, &format!("100644 blob {blob}\tsome\n"))
        }
    };
    for (index, member) in members.iter().enumerate() {
        let username = format!("member-{index}");
        let principal_blob = hash_object(&repo, username.as_bytes());
        let key = std::fs::read_to_string(member.pubkey).unwrap();
        let key_blob = hash_object(&repo, key.as_bytes());
        let keys_tree = mktree(&repo, &format!("100644 blob {key_blob}\tkey\n"));
        let trust_tree = mktree(&repo, &format!("040000 tree {keys_tree}\tKeys\n"));
        let after_tree = option_tree(member.valid_after);
        let before_tree = option_tree(member.valid_before);
        let root_tree = mktree(
            &repo,
            &format!(
                "100644 blob {principal_blob}\tprincipal\n\
                 040000 tree {after_tree}\tvalid_after\n\
                 040000 tree {before_tree}\tvalid_before\n\
                 040000 tree {trust_tree}\ttrust\n"
            ),
        );
        let commit = ok(&repo, "git", &["commit-tree", &root_tree, "-m", "member"]);
        ok(
            &repo,
            "git",
            &[
                "update-ref",
                &format!("refs/meta/member/{username}"),
                &commit,
            ],
        );
    }
    repo
}

fn hash_object(repo: &Path, bytes: &[u8]) -> String {
    pipe(repo, &["hash-object", "-w", "--stdin"], bytes)
}

fn mktree(repo: &Path, spec: &str) -> String {
    pipe(repo, &["mktree"], spec.as_bytes())
}

/// Run `git <args>` in `repo`, feeding `input` on stdin, returning trimmed stdout.
fn pipe(repo: &Path, args: &[&str], input: &[u8]) -> String {
    use std::io::Write;
    let mut child = git_env(repo, "git", args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "git {args:?} failed");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// Create a work repo with one commit, signing with `signing_key` if given.
fn work_repo(base: &Path, signing_key: Option<&Path>) -> PathBuf {
    let repo = base.join("work");
    std::fs::create_dir_all(&repo).unwrap();
    ok(&repo, "git", &["init", "-q", "-b", "main"]);
    if let Some(key) = signing_key {
        ok(&repo, "git", &["config", "gpg.format", "ssh"]);
        ok(
            &repo,
            "git",
            &["config", "user.signingkey", key.to_str().unwrap()],
        );
    }
    std::fs::write(repo.join("file.txt"), "hello\n").unwrap();
    ok(&repo, "git", &["add", "file.txt"]);
    ok(&repo, "git", &["commit", "-q", "-m", "initial"]);
    repo
}

/// Attempt a push, returning whether it succeeded.
fn push(work: &Path, server: &Path, signed: bool) -> bool {
    let url = format!("file://{}", server.display());
    let mut args = vec!["push"];
    if signed {
        args.push("--signed");
    }
    args.extend_from_slice(&[url.as_str(), "main:refs/heads/main"]);
    git_env(work, "git", &args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
}

#[test]
fn accepts_a_push_signed_by_an_authorized_key() {
    let base = unique_dir("accept");
    let pubkey = keygen(&base, "id");
    let server = server_repo(&base, &[&pubkey]);
    let work = work_repo(&base, Some(&pubkey));

    assert!(
        push(&work, &server, true),
        "authorized signed push was rejected"
    );
    std::fs::remove_dir_all(&base).ok();
}

#[test]
fn rejects_a_push_signed_by_an_unknown_key() {
    let base = unique_dir("unknown");
    let authorized = keygen(&base, "authorized");
    let intruder = keygen(&base, "intruder");
    let server = server_repo(&base, &[&authorized]);
    let work = work_repo(&base, Some(&intruder));

    assert!(
        !push(&work, &server, true),
        "push by an unknown key was accepted"
    );
    std::fs::remove_dir_all(&base).ok();
}

#[test]
fn rejects_an_unsigned_push_when_signers_exist() {
    let base = unique_dir("unsigned");
    let pubkey = keygen(&base, "id");
    let server = server_repo(&base, &[&pubkey]);
    let work = work_repo(&base, None);

    assert!(!push(&work, &server, false), "unsigned push was accepted");
    std::fs::remove_dir_all(&base).ok();
}

#[test]
fn accepts_a_push_signed_by_an_in_window_key() {
    let base = unique_dir("inwindow");
    let pubkey = keygen(&base, "id");
    let server = server_repo_with(
        &base,
        &[Member {
            pubkey: &pubkey,
            valid_after: Some("20200101"),
            valid_before: Some("20990101"),
        }],
    );
    let work = work_repo(&base, Some(&pubkey));

    assert!(
        push(&work, &server, true),
        "in-window signed push was rejected"
    );
    std::fs::remove_dir_all(&base).ok();
}

#[test]
fn rejects_a_push_signed_by_an_expired_key() {
    // The window lapsed before today, so the key no longer authorizes a new
    // push — staleness fails closed. This is the Phase 1 security gate: if the
    // verifier ignored `valid-before`, this push would be accepted.
    let base = unique_dir("expired");
    let pubkey = keygen(&base, "id");
    let server = server_repo_with(
        &base,
        &[Member {
            pubkey: &pubkey,
            valid_after: None,
            valid_before: Some("20200101"),
        }],
    );
    let work = work_repo(&base, Some(&pubkey));

    assert!(
        !push(&work, &server, true),
        "push signed by an expired-window key was accepted"
    );
    std::fs::remove_dir_all(&base).ok();
}

#[test]
fn rejects_a_push_signed_before_a_keys_window_opens() {
    let base = unique_dir("future");
    let pubkey = keygen(&base, "id");
    let server = server_repo_with(
        &base,
        &[Member {
            pubkey: &pubkey,
            valid_after: Some("20990101"),
            valid_before: None,
        }],
    );
    let work = work_repo(&base, Some(&pubkey));

    assert!(
        !push(&work, &server, true),
        "push signed before the key's window opened was accepted"
    );
    std::fs::remove_dir_all(&base).ok();
}

#[test]
fn accepts_any_push_before_signers_are_configured() {
    let base = unique_dir("bootstrap");
    let server = server_repo(&base, &[]);
    let work = work_repo(&base, None);

    assert!(push(&work, &server, false), "bootstrap push was rejected");
    std::fs::remove_dir_all(&base).ok();
}
