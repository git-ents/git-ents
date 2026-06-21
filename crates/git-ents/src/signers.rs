//! The authorized signer set, sourced from the `refs/meta/auth` ref.
//!
//! Push authentication trusts exactly one place: the `refs/meta/auth` ref. Each
//! blob under its `signers/` tree is one authorized OpenSSH public key, stored
//! under a name that is the key's fingerprint. Because the set lives in a ref,
//! the trust list is versioned, auditable, and itself pushable.

use std::path::Path;
use std::process::Command;

/// The ref whose tree holds the authorized signer set.
pub const AUTH_REF: &str = "refs/meta/auth";

/// One authorized signer recorded under `signers/` in [`AUTH_REF`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signer {
    /// The `signers/<name>` the key is stored under — its fingerprint.
    pub fingerprint: String,
    /// The OpenSSH public key the blob holds (`<type> <base64> [comment]`).
    pub key: String,
}

/// Load the authorized signers recorded at [`AUTH_REF`] in the repository at
/// `repo`.
///
/// Returns an empty set when the ref or its `signers/` tree is absent, as on a
/// fresh server whose trust list has not been pushed yet.
#[must_use]
pub fn load(repo: &Path) -> Vec<Signer> {
    let Some(listing) = git(repo, &["ls-tree", &format!("{AUTH_REF}:signers")]) else {
        return Vec::new();
    };
    listing
        .lines()
        .filter_map(parse_blob_entry)
        .filter_map(|(oid, fingerprint)| {
            let key = git(repo, &["cat-file", "blob", oid])?;
            Some(Signer {
                fingerprint: fingerprint.to_owned(),
                key: key.trim_end().to_owned(),
            })
        })
        .collect()
}

/// Render `signers` as an OpenSSH `allowed_signers` file that authorizes any
/// pusher identity (`*`) signing in git's namespace.
///
/// The principal is a wildcard because authentication here is membership of the
/// key set, not a binding between a key and a particular identity: `ssh-keygen
/// -Y verify` accepts the push certificate as long as the signing key is one of
/// these, whatever name the pusher signed under.
#[must_use]
pub fn allowed_signers(signers: &[Signer]) -> String {
    signers
        .iter()
        .map(|signer| format!("* namespaces=\"git\" {}\n", signer.key))
        .collect()
}

/// Parse one `git ls-tree` line (`<mode> SP <type> SP <oid> TAB <name>`),
/// yielding `(oid, name)` only for blob entries so nested trees are skipped.
fn parse_blob_entry(line: &str) -> Option<(&str, &str)> {
    let (meta, name) = line.split_once('\t')?;
    let mut columns = meta.split_whitespace();
    let _mode = columns.next()?;
    if columns.next()? != "blob" {
        return None;
    }
    let oid = columns.next()?;
    Some((oid, name))
}

/// Run `git -C <repo> <args>` and return its stdout as a string, or `None` when
/// git fails or the output is not UTF-8.
fn git(repo: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::panic,
        clippy::arithmetic_side_effects,
        clippy::let_underscore_must_use,
        reason = "unit test"
    )]

    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    const KEY_A: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaA alice";
    const KEY_B: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbB bob";

    fn unique_dir() -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("git-ents-signers-{}-{n}", std::process::id()))
    }

    fn run(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@e")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@e")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    /// Build a repo whose `refs/meta/auth` carries `signers/<name>` blobs.
    fn repo_with_signers(entries: &[(&str, &str)]) -> PathBuf {
        let dir = unique_dir();
        std::fs::create_dir_all(dir.join("signers")).unwrap();
        run(&dir, &["init", "-q"]);
        for (name, key) in entries {
            std::fs::write(dir.join("signers").join(name), format!("{key}\n")).unwrap();
        }
        run(&dir, &["add", "signers"]);
        let tree = capture(&dir, &["write-tree"]);
        let commit = capture(&dir, &["commit-tree", &tree, "-m", "auth"]);
        run(&dir, &["update-ref", AUTH_REF, &commit]);
        dir
    }

    fn capture(dir: &Path, args: &[&str]) -> String {
        git(dir, args).unwrap().trim().to_owned()
    }

    #[test]
    fn loads_signers_from_the_auth_ref() {
        let dir = repo_with_signers(&[("SHA256-aaa", KEY_A), ("SHA256-bbb", KEY_B)]);
        let mut signers = load(&dir);
        signers.sort_by(|a, b| a.fingerprint.cmp(&b.fingerprint));
        assert_eq!(
            signers,
            vec![
                Signer {
                    fingerprint: "SHA256-aaa".to_owned(),
                    key: KEY_A.to_owned()
                },
                Signer {
                    fingerprint: "SHA256-bbb".to_owned(),
                    key: KEY_B.to_owned()
                },
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_when_the_auth_ref_is_absent() {
        let dir = unique_dir();
        std::fs::create_dir_all(&dir).unwrap();
        run(&dir, &["init", "-q"]);
        assert!(load(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn renders_a_wildcard_allowed_signers_file() {
        let signers = vec![Signer {
            fingerprint: "SHA256-aaa".to_owned(),
            key: KEY_A.to_owned(),
        }];
        assert_eq!(
            allowed_signers(&signers),
            format!("* namespaces=\"git\" {KEY_A}\n")
        );
    }
}
