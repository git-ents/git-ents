//! Test-only git fixtures: a throwaway repository and the plumbing helpers
//! this crate's test suites drive it with (ported from `pre-redo`'s
//! `git-store::test_support`).
//!
//! Compiled only under `cfg(test)`. When a second crate needs these
//! helpers, they move to the shared `ents-testutil` dev-dependency crate
//! the workspace test strategy calls for; extracting them now would create
//! a crate no second consumer exists for yet.

#![allow(clippy::unwrap_used, reason = "test fixture")]

use std::path::Path;
use std::process::Command;

/// A fresh temporary directory holding an initialized git repository.
#[must_use]
pub(crate) fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let status = Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .args(["init", "-q"])
        .status()
        .unwrap();
    assert!(status.success());
    for (key, value) in [("user.email", "test@example.com"), ("user.name", "test")] {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["config", key, value])
            .status()
            .unwrap();
        assert!(status.success());
    }
    dir
}

/// Stage everything in `dir` and commit it as `message` under the fixed
/// test identity.
pub(crate) fn commit_all(dir: &Path, message: &str) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["add", "-A"])
        .status()
        .unwrap();
    assert!(status.success());
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-q",
            "-m",
            message,
        ])
        .status()
        .unwrap();
    assert!(status.success());
}

/// The full hex id of `dir`'s `HEAD` commit.
#[must_use]
pub(crate) fn head(dir: &Path) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// `range.map(|n| "line {n}\n")` concatenated — the numbered fixture file
/// every projection test edits.
#[must_use]
pub(crate) fn numbered(range: std::ops::RangeInclusive<u32>) -> String {
    range.map(|n| format!("line {n}\n")).collect()
}
