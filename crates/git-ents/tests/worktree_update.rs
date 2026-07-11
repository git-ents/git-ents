//! `roots.worktree-update`: `git ents setup` sets
//! `receive.denyCurrentBranch=updateInstead` on the local repository, so
//! the integration-test-harness case — an external push landing on this
//! repository's own checked-out branch — also updates the working tree,
//! rather than the ordinary git behavior of refusing such a push outright.

#![allow(clippy::expect_used, reason = "integration test")]

mod common;

use std::path::Path;
use std::process::Command;

use git_ents::commands;
use git_ents::root::LocalRoot;

fn git(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@ents.test")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@ents.test")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env_remove("HOME")
        .output()
        .expect("git runs")
}

/// After `git ents setup`, an external push into this repository's own
/// checked-out branch both succeeds and updates the working tree —
/// `receive.denyCurrentBranch=updateInstead` in effect
/// (`roots.worktree-update`).
// @relation(roots.worktree-update, scope=function, role=Verifies)
#[test]
fn setup_lets_a_harness_push_update_the_checked_out_branch() {
    let origin_dir = tempfile::tempdir().expect("tempdir");
    let init = git(origin_dir.path(), &["init", "-q", "-b", "main"]);
    assert!(init.status.success(), "{init:?}");
    std::fs::write(origin_dir.path().join("file.txt"), "before\n").expect("write");
    let add = git(origin_dir.path(), &["add", "-A"]);
    assert!(add.status.success());
    let commit = git(origin_dir.path(), &["commit", "-q", "-m", "before"]);
    assert!(commit.status.success(), "{commit:?}");

    // `git ents setup` is what this test exercises: it must record
    // `receive.denyCurrentBranch=updateInstead` on `origin_dir`'s own
    // config. An explicit `--key` sidesteps `resolve_key_path`'s
    // ambient-fallback resolution (`user.signingkey`, `~/.ssh/id_ed25519`)
    // entirely, keeping this test isolated from whatever the machine
    // running it happens to have configured globally, without mutating
    // process environment (`unsafe_code` is workspace-forbidden even in
    // tests).
    let key_path = common::write_key_in(origin_dir.path(), 30);
    let root = LocalRoot::open(origin_dir.path()).expect("opens");
    commands::setup::run(&root, Some(key_path)).expect("configures the repo");

    let config = git(origin_dir.path(), &["config", "receive.denyCurrentBranch"]);
    assert_eq!(
        String::from_utf8_lossy(&config.stdout).trim(),
        "updateInstead",
        "{config:?}"
    );

    // An external clone pushes a change directly onto `main` — the
    // integration-test-harness case `roots.worktree-update` names.
    let clone_dir = tempfile::tempdir().expect("tempdir");
    let clone = git(
        clone_dir.path(),
        &[
            "clone",
            "--quiet",
            origin_dir.path().to_str().expect("utf8"),
            ".",
        ],
    );
    assert!(clone.status.success(), "{clone:?}");
    std::fs::write(clone_dir.path().join("file.txt"), "after\n").expect("write");
    let add = git(clone_dir.path(), &["add", "-A"]);
    assert!(add.status.success());
    let commit = git(clone_dir.path(), &["commit", "-q", "-m", "after"]);
    assert!(commit.status.success(), "{commit:?}");

    let push = git(clone_dir.path(), &["push", "origin", "main"]);
    assert!(
        push.status.success(),
        "a push into the checked-out branch must be accepted, not refused: {push:?}"
    );

    // The working tree on `origin_dir` reflects the push, not just its
    // ref — this is the "also updates the working tree" half of
    // `updateInstead`, distinct from an ordinary bare remote.
    let updated = std::fs::read_to_string(origin_dir.path().join("file.txt")).expect("read");
    assert_eq!(
        updated, "after\n",
        "the checked-out working tree must update"
    );
}
