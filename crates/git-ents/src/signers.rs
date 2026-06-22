//! The authorized signer set, sourced from the `refs/meta/auth` ref.
//!
//! Push authentication trusts exactly one place: the `refs/meta/auth` ref. Its
//! tree is an [`Auth`] document whose `signers/` subtree maps each fingerprint
//! to its OpenSSH public key. The document is read and written with
//! [`facet_git_tree`], so the trust list is a typed value that lives in git —
//! versioned, auditable, and itself pushable.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use facet::Facet;
use facet_git_tree::ObjectId;

/// The ref whose tree holds the authorized signer set.
pub const AUTH_REF: &str = "refs/meta/auth";

/// The authorization document stored at [`AUTH_REF`]: `signers/<fingerprint>`
/// maps to the OpenSSH public key held there.
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
struct Auth {
    signers: BTreeMap<String, String>,
}

/// One authorized signer recorded under `signers/` in [`AUTH_REF`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signer {
    /// The `signers/<name>` the key is stored under — its fingerprint.
    pub fingerprint: String,
    /// The OpenSSH public key the blob holds (`<type> <base64> [comment]`).
    pub key: String,
}

/// A failure reading or writing the signer set.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The repository's object database could not be opened.
    #[error("could not open the repository object database")]
    Odb,
    /// The signer set could not be (de)serialized from its git tree.
    #[error("could not (de)serialize the signer set: {0}")]
    Facet(#[from] facet_git_tree::Error),
    /// A git invocation needed to read or update the ref failed.
    #[error("git {operation} failed")]
    Git {
        /// The git operation that failed.
        operation: &'static str,
    },
}

/// Load the authorized signers recorded at [`AUTH_REF`] in `repo`.
///
/// An absent ref yields an empty set, as on a fresh server whose trust list has
/// not been pushed yet. A present but unreadable ref is an error so callers can
/// fail closed rather than mistake corruption for "no signers".
pub fn load(repo: &Path) -> Result<Vec<Signer>, Error> {
    let Some(tree) = auth_tree(repo) else {
        return Ok(Vec::new());
    };
    let odb = open_odb(repo).ok_or(Error::Odb)?;
    let auth: Auth = facet_git_tree::deserialize(&tree, &odb)?;
    Ok(auth
        .signers
        .into_iter()
        .map(|(fingerprint, key)| Signer {
            fingerprint,
            key: key.trim_end().to_owned(),
        })
        .collect())
}

/// Write `signers` to [`AUTH_REF`], replacing any existing set, as a new commit.
pub fn store(repo: &Path, signers: &[Signer]) -> Result<(), Error> {
    let auth = Auth {
        signers: signers
            .iter()
            .map(|signer| (signer.fingerprint.clone(), signer.key.clone()))
            .collect(),
    };
    let odb = open_odb(repo).ok_or(Error::Odb)?;
    let tree = facet_git_tree::serialize_into(&auth, &odb)?;
    let commit = commit_tree(repo, &tree)?;
    update_ref(repo, &commit)
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

/// Resolve [`AUTH_REF`] to the object id of its tree, or `None` when the ref is
/// absent.
fn auth_tree(repo: &Path) -> Option<ObjectId> {
    let spec = format!("{AUTH_REF}^{{tree}}");
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", "--quiet", &spec])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let hex = String::from_utf8(output.stdout).ok()?;
    ObjectId::from_hex(hex.trim().as_bytes()).ok()
}

/// Open the repository's durable object database as a `gix` `Find`/`Write`
/// backend.
///
/// Resolves the *common* git directory rather than `--git-path objects`: inside
/// a `pre-receive` hook git points the latter at a quarantine holding only the
/// incoming pack, while the current signer set lives in the durable store — and
/// authorization is against the pre-push set, never the keys being pushed.
fn open_odb(repo: &Path) -> Option<gix_odb::Handle> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--git-common-dir"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let git_dir = String::from_utf8(output.stdout).ok()?;
    gix_odb::at(repo.join(git_dir.trim()).join("objects")).ok()
}

/// Wrap `tree` in a commit, returning its object id. A fixed identity keeps the
/// write self-contained, independent of any ambient git config.
fn commit_tree(repo: &Path, tree: &ObjectId) -> Result<String, Error> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "commit-tree",
            &tree.to_string(),
            "-m",
            "Update authorized signers",
        ])
        .env("GIT_AUTHOR_NAME", "git-ents")
        .env("GIT_AUTHOR_EMAIL", "git-ents@localhost")
        .env("GIT_COMMITTER_NAME", "git-ents")
        .env("GIT_COMMITTER_EMAIL", "git-ents@localhost")
        .output()
        .map_err(|_source| Error::Git {
            operation: "commit-tree",
        })?;
    if !output.status.success() {
        return Err(Error::Git {
            operation: "commit-tree",
        });
    }
    String::from_utf8(output.stdout)
        .map(|stdout| stdout.trim().to_owned())
        .map_err(|_invalid| Error::Git {
            operation: "commit-tree",
        })
}

/// Point [`AUTH_REF`] at `commit`.
fn update_ref(repo: &Path, commit: &str) -> Result<(), Error> {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["update-ref", AUTH_REF, commit])
        .status()
        .map_err(|_source| Error::Git {
            operation: "update-ref",
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::Git {
            operation: "update-ref",
        })
    }
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

    fn unique_repo() -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("git-ents-signers-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let status = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["init", "-q"])
            .status()
            .unwrap();
        assert!(status.success());
        dir
    }

    fn signer(fingerprint: &str, key: &str) -> Signer {
        Signer {
            fingerprint: fingerprint.to_owned(),
            key: key.to_owned(),
        }
    }

    #[test]
    fn store_then_load_round_trips_the_signer_set() {
        let repo = unique_repo();
        let written = vec![signer("SHA256-aaa", KEY_A), signer("SHA256-bbb", KEY_B)];
        store(&repo, &written).unwrap();

        let mut loaded = load(&repo).unwrap();
        loaded.sort_by(|a, b| a.fingerprint.cmp(&b.fingerprint));
        assert_eq!(loaded, written);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn store_replaces_the_previous_set() {
        let repo = unique_repo();
        store(&repo, &[signer("SHA256-aaa", KEY_A)]).unwrap();
        store(&repo, &[signer("SHA256-bbb", KEY_B)]).unwrap();
        assert_eq!(load(&repo).unwrap(), vec![signer("SHA256-bbb", KEY_B)]);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn empty_when_the_auth_ref_is_absent() {
        let repo = unique_repo();
        assert!(load(&repo).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn renders_a_wildcard_allowed_signers_file() {
        assert_eq!(
            allowed_signers(&[signer("SHA256-aaa", KEY_A)]),
            format!("* namespaces=\"git\" {KEY_A}\n")
        );
    }
}
