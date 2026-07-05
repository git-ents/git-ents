//! Shared test helpers for the member and revocation modules: a throwaway git
//! repository and a builder that lays an on-disk `refs/meta/*` document out
//! with raw git plumbing.
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

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A freshly initialized, uniquely named git repository under the temp dir.
#[must_use]
pub(crate) fn unique_repo(label: &str) -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("git-member-{label}-{}-{n}", std::process::id()));
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

/// Lay a `Member` document out at `refs/meta/member/<username>` as the real
/// on-disk format: a `principal` blob, `valid_after`/`valid_before` `Option`
/// subtrees (empty tree for `None`, a single `some` blob for a bound), and a
/// `trust/Keys/<fingerprint>` blob per key (the `Trust::Keys` newtype enum
/// variant resolving directly to its map). Asserts the loader still reads the
/// format independent of the writer.
pub(crate) fn write_member_doc(
    repo: &Path,
    username: &str,
    valid_after: Option<&str>,
    valid_before: Option<&str>,
    keys: &[(&str, &str)],
) {
    let option_tree = |bound: Option<&str>| match bound {
        None => git_with_stdin(repo, &["mktree"], ""),
        Some(value) => {
            let blob = git_with_stdin(repo, &["hash-object", "-w", "--stdin"], value);
            git_with_stdin(repo, &["mktree"], &format!("100644 blob {blob}\tsome\n"))
        }
    };
    let principal_blob = git_with_stdin(repo, &["hash-object", "-w", "--stdin"], username);
    let after_tree = option_tree(valid_after);
    let before_tree = option_tree(valid_before);
    let mut key_entries = String::new();
    for (fingerprint, key) in keys {
        let key_blob = git_with_stdin(repo, &["hash-object", "-w", "--stdin"], key);
        key_entries.push_str(&format!("100644 blob {key_blob}\t{fingerprint}\n"));
    }
    let keys_tree = git_with_stdin(repo, &["mktree"], &key_entries);
    let trust_tree = git_with_stdin(
        repo,
        &["mktree"],
        &format!("040000 tree {keys_tree}\tKeys\n"),
    );
    let root = git_with_stdin(
        repo,
        &["mktree"],
        &format!(
            "100644 blob {principal_blob}\tprincipal\n\
             040000 tree {after_tree}\tvalid_after\n\
             040000 tree {before_tree}\tvalid_before\n\
             040000 tree {trust_tree}\ttrust\n"
        ),
    );
    let commit = git_with_stdin(repo, &["commit-tree", &root, "-m", "fixture"], "");
    let refname = format!("refs/meta/member/{username}");
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["update-ref", &refname, &commit])
        .status()
        .unwrap();
    assert!(status.success());
}

/// Lay a `Member` document out at `refs/meta/member/<username>` with
/// `Trust::WebAuthn` credentials and an explicit `provenance`, as the real
/// on-disk format: a `trust/WebAuthn/<credential_id>/{cose_key,label}`
/// subtree per credential (the `Trust::WebAuthn` newtype variant resolving
/// directly to its map, each `WebAuthnKey` a two-field subtree) and a
/// `provenance/<variant>` unit-variant tree. Asserts the loader still reads
/// the format independent of the writer.
pub(crate) fn write_webauthn_member_doc(
    repo: &Path,
    username: &str,
    provenance: &str,
    credentials: &[(&str, &str, &str)],
) {
    let empty_tree = git_with_stdin(repo, &["mktree"], "");
    let principal_blob = git_with_stdin(repo, &["hash-object", "-w", "--stdin"], username);
    let mut cred_entries = String::new();
    for (credential_id, cose_key, label) in credentials {
        let case_blob = git_with_stdin(repo, &["hash-object", "-w", "--stdin"], cose_key);
        let label_blob = git_with_stdin(repo, &["hash-object", "-w", "--stdin"], label);
        let cred_tree = git_with_stdin(
            repo,
            &["mktree"],
            &format!("100644 blob {case_blob}\tcose_key\n100644 blob {label_blob}\tlabel\n"),
        );
        cred_entries.push_str(&format!("040000 tree {cred_tree}\t{credential_id}\n"));
    }
    let creds_tree = git_with_stdin(repo, &["mktree"], &cred_entries);
    let trust_tree = git_with_stdin(
        repo,
        &["mktree"],
        &format!("040000 tree {creds_tree}\tWebAuthn\n"),
    );
    let provenance_tree = git_with_stdin(
        repo,
        &["mktree"],
        &format!("040000 tree {empty_tree}\t{provenance}\n"),
    );
    let root = git_with_stdin(
        repo,
        &["mktree"],
        &format!(
            "100644 blob {principal_blob}\tprincipal\n\
             040000 tree {empty_tree}\tvalid_after\n\
             040000 tree {empty_tree}\tvalid_before\n\
             040000 tree {trust_tree}\ttrust\n\
             040000 tree {provenance_tree}\tprovenance\n"
        ),
    );
    let commit = git_with_stdin(repo, &["commit-tree", &root, "-m", "fixture"], "");
    let refname = format!("refs/meta/member/{username}");
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["update-ref", &refname, &commit])
        .status()
        .unwrap();
    assert!(status.success());
}

/// Lay a `Revocations` document out at [`crate::revocations::REVOKED_REF`] as
/// the real on-disk format after the revocations migration, at the ref's tree
/// root — a bare scalar-keyed map, no wrapper struct: a `<fingerprint>/reason`
/// blob per entry (the map value is a `RevocationBody` subtree, not a bare
/// blob). Asserts the loader still reads the format independent of the
/// writer.
pub(crate) fn write_revocations_doc(repo: &Path, revoked: &[(&str, &str)]) {
    let mut entries = String::new();
    for (fingerprint, reason) in revoked {
        let reason_blob = git_with_stdin(repo, &["hash-object", "-w", "--stdin"], reason);
        let body_tree = git_with_stdin(
            repo,
            &["mktree"],
            &format!("100644 blob {reason_blob}\treason\n"),
        );
        entries.push_str(&format!("040000 tree {body_tree}\t{fingerprint}\n"));
    }
    let revoked_tree = git_with_stdin(repo, &["mktree"], &entries);
    let commit = git_with_stdin(repo, &["commit-tree", &revoked_tree, "-m", "fixture"], "");
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["update-ref", crate::revocations::REVOKED_REF, &commit])
        .status()
        .unwrap();
    assert!(status.success());
}

/// Run git in `repo` with `input` on stdin, returning its trimmed stdout.
fn git_with_stdin(repo: &Path, args: &[&str], input: &str) -> String {
    git_store::test_support::git_with_stdin(repo, args, input)
}
