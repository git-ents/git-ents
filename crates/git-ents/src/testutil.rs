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

use std::path::{Path, PathBuf};
use std::process::Command;
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

/// Lay an `Account` document out at `refs/meta/account` as the real on-disk
/// format: `username`, `display_name`, `bio`, and `created_at` blobs (the
/// integer in its decimal `Display` form). Asserts the loader still reads the
/// format independent of the writer.
pub(crate) fn write_account_doc(
    repo: &Path,
    username: &str,
    display_name: &str,
    bio: &str,
    created_at: u64,
) {
    let blob = |value: &str| git_with_stdin(repo, &["hash-object", "-w", "--stdin"], value);
    let username_blob = blob(username);
    let display_blob = blob(display_name);
    let bio_blob = blob(bio);
    let created_blob = blob(&created_at.to_string());
    let root = git_with_stdin(
        repo,
        &["mktree"],
        &format!(
            "100644 blob {created_blob}\tcreated_at\n\
             100644 blob {display_blob}\tdisplay_name\n\
             100644 blob {bio_blob}\tbio\n\
             100644 blob {username_blob}\tusername\n"
        ),
    );
    let commit = git_with_stdin(repo, &["commit-tree", &root, "-m", "fixture"], "");
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["update-ref", "refs/meta/account", &commit])
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
/// `title`, `body`, and `author` blobs, a `state/<Variant>` subtree (the
/// `State` enum's unit variant resolving to an empty tree, exactly like
/// `Member`'s `provenance`), and an index-keyed (`0000`, `0001`, …) `labels/`
/// subtree, committed and pointed to by the ref. `state` is the `State`
/// variant's name (`"Open"`, `"Closed"`). Asserts the loader still reads the
/// format independent of the writer.
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
    let author_blob = blob(author);
    let empty_tree = git_with_stdin(repo, &["mktree"], "");
    let state_tree = git_with_stdin(
        repo,
        &["mktree"],
        &format!("040000 tree {empty_tree}\t{state}\n"),
    );
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
             040000 tree {state_tree}\tstate\n\
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

/// Lay the checks document out at [`crate::checks::CHECKS_REF`] as the real
/// on-disk format: a bare scalar-keyed map at the ref's tree root, one
/// `<name>/command/some` blob per configured check (the map value is a
/// `CheckBody` subtree whose `command` is the `Option` tree encoding, and the
/// map itself is the whole document — no wrapper struct), with the optional
/// `image`/`depends` fields omitted entirely — asserting the loader fills a
/// check's missing optional fields as unset, independent of the writer.
pub(crate) fn write_checks_doc(repo: &Path, checks: &[(&str, &str)]) {
    let mut entries = String::new();
    for (name, command) in checks {
        let command_blob = git_with_stdin(repo, &["hash-object", "-w", "--stdin"], command);
        let some_tree = git_with_stdin(
            repo,
            &["mktree"],
            &format!("100644 blob {command_blob}\tsome\n"),
        );
        let check_tree = git_with_stdin(
            repo,
            &["mktree"],
            &format!("040000 tree {some_tree}\tcommand\n"),
        );
        entries.push_str(&format!("040000 tree {check_tree}\t{name}\n"));
    }
    let checks_tree = git_with_stdin(repo, &["mktree"], &entries);
    let commit = git_with_stdin(repo, &["commit-tree", &checks_tree, "-m", "fixture"], "");
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["update-ref", crate::checks::CHECKS_REF, &commit])
        .status()
        .unwrap();
    assert!(status.success());
}

/// Lay a `results/<name>` run-outcomes document out at `refname` as the real
/// on-disk format: a `results/<name>/status/<Variant>` subtree per outcome
/// (the `Status` enum's unit variant resolving to an empty tree, exactly like
/// `Member`'s `provenance`), with `duration_secs`/`recording` omitted entirely —
/// asserting the loader fills a record's missing optional fields as unset,
/// independent of the writer. `variant` is the `Status` variant's name
/// (`"Pass"`, `"Fail"`, …).
pub(crate) fn write_runs_doc(repo: &Path, refname: &str, outcomes: &[(&str, &str)]) {
    let empty_tree = git_with_stdin(repo, &["mktree"], "");
    let mut entries = String::new();
    for (name, variant) in outcomes {
        let variant_tree = git_with_stdin(
            repo,
            &["mktree"],
            &format!("040000 tree {empty_tree}\t{variant}\n"),
        );
        let outcome_tree = git_with_stdin(
            repo,
            &["mktree"],
            &format!("040000 tree {variant_tree}\tstatus\n"),
        );
        entries.push_str(&format!("040000 tree {outcome_tree}\t{name}\n"));
    }
    let results_tree = git_with_stdin(repo, &["mktree"], &entries);
    let commit = git_with_stdin(repo, &["commit-tree", &results_tree, "-m", "fixture"], "");
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["update-ref", refname, &commit])
        .status()
        .unwrap();
    assert!(status.success());
}

/// Lay a `Revocations` document out at
/// [`crate::revocations::REVOKED_REF`] as the real on-disk format after the
/// revocations migration, at the ref's tree root — a bare scalar-keyed map,
/// no wrapper struct: a `<fingerprint>/reason` blob per entry (the map value
/// is a `RevocationBody` subtree, not a bare blob). Asserts the loader still
/// reads the format independent of the writer.
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
