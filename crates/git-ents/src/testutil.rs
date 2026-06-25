//! Shared test helpers for the meta-ref modules: a throwaway git repository and
//! a builder that lays an on-disk `refs/meta/*` document out with raw git
//! plumbing.
//!
//! Building the tree directly — rather than through [`git_store::Store`] — pins
//! the *on-disk* layout each document type promises: a `<subtree>/<key>` blob
//! per entry. A load test against a fixture written this way fails the moment an
//! incompatible change to a document's [`facet::Facet`] shape stops reading data
//! already in the wild, the failure mode that broke every push once before.

#![allow(
    clippy::unwrap_used,
    clippy::let_underscore_must_use,
    reason = "test support"
)]

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

/// A freshly initialized, uniquely named git repository under the temp dir.
#[must_use]
pub(crate) fn unique_repo(label: &str) -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("git-ents-{label}-{}-{n}", std::process::id()));
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

/// Lay a document out at `refname` as the real on-disk format: one
/// `<subtree>/<key>` blob per pair, committed and pointed to by the ref. Used to
/// assert that loaders still read the format independent of the writer.
pub(crate) fn write_meta_doc(repo: &Path, refname: &str, subtree: &str, pairs: &[(&str, &str)]) {
    let mut entries = String::new();
    for (key, value) in pairs {
        let blob = git_with_stdin(repo, &["hash-object", "-w", "--stdin"], value);
        entries.push_str(&format!("100644 blob {blob}\t{key}\n"));
    }
    let sub = git_with_stdin(repo, &["mktree"], &entries);
    let root = git_with_stdin(
        repo,
        &["mktree"],
        &format!("040000 tree {sub}\t{subtree}\n"),
    );
    let commit = git_with_stdin(repo, &["commit-tree", &root, "-m", "fixture"], "");
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["update-ref", refname, &commit])
        .status()
        .unwrap();
    assert!(status.success());
}

/// Lay a `Members` document out at `refname` as the real on-disk format: a
/// `members/<fingerprint>/` subtree per member holding a `key` blob and an
/// `valid_after`/`valid_before` `Option` subtree each (empty tree for `None`, a
/// single `some` blob for a bound). Asserts the loader still reads the format
/// independent of the writer.
pub(crate) fn write_members_doc(
    repo: &Path,
    refname: &str,
    members: &[(&str, &str, Option<&str>, Option<&str>)],
) {
    let option_tree = |bound: Option<&str>| match bound {
        None => git_with_stdin(repo, &["mktree"], ""),
        Some(value) => {
            let blob = git_with_stdin(repo, &["hash-object", "-w", "--stdin"], value);
            git_with_stdin(repo, &["mktree"], &format!("100644 blob {blob}\tsome\n"))
        }
    };
    let mut member_entries = String::new();
    for (fingerprint, key, valid_after, valid_before) in members {
        let key_blob = git_with_stdin(repo, &["hash-object", "-w", "--stdin"], key);
        let after_tree = option_tree(*valid_after);
        let before_tree = option_tree(*valid_before);
        let member_tree = git_with_stdin(
            repo,
            &["mktree"],
            &format!(
                "100644 blob {key_blob}\tkey\n\
                 040000 tree {after_tree}\tvalid_after\n\
                 040000 tree {before_tree}\tvalid_before\n"
            ),
        );
        member_entries.push_str(&format!("040000 tree {member_tree}\t{fingerprint}\n"));
    }
    let members_tree = git_with_stdin(repo, &["mktree"], &member_entries);
    let root = git_with_stdin(
        repo,
        &["mktree"],
        &format!("040000 tree {members_tree}\tmembers\n"),
    );
    let commit = git_with_stdin(repo, &["commit-tree", &root, "-m", "fixture"], "");
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["update-ref", refname, &commit])
        .status()
        .unwrap();
    assert!(status.success());
}

/// Lay a `Config` document out at `refname` as the real on-disk format: a
/// `description` blob, a `homepage` blob, and a `topics/` subtree of index-keyed
/// (`0000`, `0001`, …) blobs, committed and pointed to by the ref. Asserts the
/// loader still reads the format independent of the writer.
pub(crate) fn write_config_doc(
    repo: &Path,
    refname: &str,
    description: &str,
    homepage: &str,
    topics: &[&str],
) {
    let description_blob = git_with_stdin(repo, &["hash-object", "-w", "--stdin"], description);
    let homepage_blob = git_with_stdin(repo, &["hash-object", "-w", "--stdin"], homepage);
    let mut topic_entries = String::new();
    for (index, topic) in topics.iter().enumerate() {
        let blob = git_with_stdin(repo, &["hash-object", "-w", "--stdin"], topic);
        topic_entries.push_str(&format!("100644 blob {blob}\t{index:04}\n"));
    }
    let topics_tree = git_with_stdin(repo, &["mktree"], &topic_entries);
    let root = git_with_stdin(
        repo,
        &["mktree"],
        &format!(
            "100644 blob {description_blob}\tdescription\n\
             100644 blob {homepage_blob}\thomepage\n\
             040000 tree {topics_tree}\ttopics\n"
        ),
    );
    let commit = git_with_stdin(repo, &["commit-tree", &root, "-m", "fixture"], "");
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["update-ref", refname, &commit])
        .status()
        .unwrap();
    assert!(status.success());
}

/// Lay an `Issue` document out at `refname` as the real on-disk format:
/// `title`, `body`, `state`, and `author` blobs plus an index-keyed (`0000`,
/// `0001`, …) `labels/` subtree, committed and pointed to by the ref. Asserts
/// the loader still reads the format independent of the writer.
pub(crate) fn write_issue_doc(
    repo: &Path,
    refname: &str,
    title: &str,
    body: &str,
    state: &str,
    labels: &[&str],
    author: &str,
) {
    let blob = |value: &str| git_with_stdin(repo, &["hash-object", "-w", "--stdin"], value);
    let title_blob = blob(title);
    let body_blob = blob(body);
    let state_blob = blob(state);
    let author_blob = blob(author);
    let mut label_entries = String::new();
    for (index, label) in labels.iter().enumerate() {
        label_entries.push_str(&format!("100644 blob {}\t{index:04}\n", blob(label)));
    }
    let labels_tree = git_with_stdin(repo, &["mktree"], &label_entries);
    let root = git_with_stdin(
        repo,
        &["mktree"],
        &format!(
            "100644 blob {title_blob}\ttitle\n\
             100644 blob {body_blob}\tbody\n\
             100644 blob {state_blob}\tstate\n\
             040000 tree {labels_tree}\tlabels\n\
             100644 blob {author_blob}\tauthor\n"
        ),
    );
    let commit = git_with_stdin(repo, &["commit-tree", &root, "-m", "fixture"], "");
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["update-ref", refname, &commit])
        .status()
        .unwrap();
    assert!(status.success());
}

/// Run git in `repo` with `input` on stdin, returning its trimmed stdout.
fn git_with_stdin(repo: &Path, args: &[&str], input: &str) -> String {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "git {args:?} failed");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}
