//! Shared test helpers: a throwaway git repository and builders that lay an
//! on-disk `refs/meta/*` document out with raw git plumbing.
//!
//! Building the tree directly — rather than through [`git_store::Store`] —
//! pins the *on-disk* layout each document type promises: a load test against
//! a fixture written this way fails the moment an incompatible change to a
//! document's [`facet::Facet`] shape stops reading data already in the wild,
//! the failure mode that broke every push once before.

#![allow(
    clippy::unwrap_used,
    clippy::let_underscore_must_use,
    reason = "test support"
)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use gix_hash::ObjectId;

use crate::definition::effect_ref;
use crate::results::RESULTS_NS;

/// A freshly initialized, uniquely named git repository under the temp dir.
#[must_use]
pub(crate) fn unique_repo(label: &str) -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("git-effect-{label}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let status = Command::new("git")
        .arg("-C")
        .arg(&dir)
        .args(["init", "-q"])
        .status()
        .unwrap();
    assert!(status.success());
    for (key, value) in [("user.email", "test@example.com"), ("user.name", "Test")] {
        let status = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["config", key, value])
            .status()
            .unwrap();
        assert!(status.success());
    }
    dir
}

/// Lay an effect document out at [`effect_ref`]`(name)` as the real on-disk
/// format: a bare `command/some` blob (the `Option`-wrapped command), with
/// the optional `image`/`depends`/`toolchains` fields omitted entirely.
/// Asserts the loader fills a missing optional field as unset, independent
/// of the writer.
pub(crate) fn write_effect_doc(repo: &Path, name: &str, command: &str) {
    let command_blob = git_with_stdin(repo, &["hash-object", "-w", "--stdin"], command);
    let some_tree = git_with_stdin(
        repo,
        &["mktree"],
        &format!("100644 blob {command_blob}\tsome\n"),
    );
    let root = git_with_stdin(
        repo,
        &["mktree"],
        &format!("040000 tree {some_tree}\tcommand\n"),
    );
    let commit = git_with_stdin(repo, &["commit-tree", &root, "-m", "fixture"], "");
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["update-ref", &effect_ref(name), &commit])
        .status()
        .unwrap();
    assert!(status.success());
}

/// Lay a result document out at `refs/meta/results/<effect>/<commit>` as the
/// real on-disk format: a `commit` blob (the checked commit's full hex id)
/// and a `status/<variant>` subtree (the `Status` enum's unit variant
/// resolving to an empty tree, exactly like `Member`'s `provenance`), with
/// `duration_secs`/`recording`/`exit_code` omitted entirely — asserting the
/// loader fills a result's missing optional fields as unset, independent of
/// the writer. `variant` is the `Status` variant's name (`"Pass"`, `"Fail"`,
/// …).
pub(crate) fn write_result_doc(repo: &Path, effect: &str, commit: ObjectId, variant: &str) {
    let commit_blob = git_with_stdin(repo, &["hash-object", "-w", "--stdin"], &commit.to_string());
    let empty_tree = git_with_stdin(repo, &["mktree"], "");
    let variant_tree = git_with_stdin(
        repo,
        &["mktree"],
        &format!("040000 tree {empty_tree}\t{variant}\n"),
    );
    let root = git_with_stdin(
        repo,
        &["mktree"],
        &format!(
            "100644 blob {commit_blob}\tcommit\n\
             040000 tree {variant_tree}\tstatus\n"
        ),
    );
    let refname = format!("{RESULTS_NS}/{effect}/{}", commit.to_hex_with_len(12));
    let tree_commit = git_with_stdin(repo, &["commit-tree", &root, "-m", "fixture"], "");
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["update-ref", &refname, &tree_commit])
        .status()
        .unwrap();
    assert!(status.success());
}

/// Run git in `repo` with `input` on stdin, returning its trimmed stdout.
fn git_with_stdin(repo: &Path, args: &[&str], input: &str) -> String {
    git_store::test_support::git_with_stdin(repo, args, input)
}
