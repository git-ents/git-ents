//! The authorized signer set, sourced from the `refs/meta/auth` ref.
//!
//! Push authentication trusts exactly one place: the `refs/meta/auth` ref. Its
//! tree is an [`Auth`] document mapping each fingerprint to its OpenSSH public
//! key. The document is read and written through [`git_store`], so the trust
//! list is a typed value that lives in git — versioned, auditable, and itself
//! pushable.

use std::collections::BTreeMap;
use std::path::Path;

use facet::Facet;

/// The ref whose tree holds the authorized signer set.
pub const AUTH_REF: &str = "refs/meta/auth";

/// The authorization document stored at [`AUTH_REF`]: its `signers/` subtree
/// maps each fingerprint to the OpenSSH public key held there.
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
struct Auth {
    signers: BTreeMap<String, String>,
}

/// One authorized signer recorded in [`AUTH_REF`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signer {
    /// The key it is stored under — its fingerprint.
    pub fingerprint: String,
    /// The OpenSSH public key the blob holds (`<type> <base64> [comment]`).
    pub key: String,
}

/// A failure reading or writing the signer set.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The signer set could not be read from or written to its ref.
    #[error(transparent)]
    Store(#[from] git_store::Error),
}

/// Load the authorized signers recorded at [`AUTH_REF`] in `repo`.
///
/// An absent ref yields an empty set, as on a fresh server whose trust list has
/// not been pushed yet. A present but unreadable ref is an error so callers can
/// fail closed rather than mistake corruption for "no signers".
pub fn load(repo: &Path) -> Result<Vec<Signer>, Error> {
    let store = git_store::Store::open(repo)?;
    let Some(auth) = store.load::<Auth>(AUTH_REF)? else {
        return Ok(Vec::new());
    };
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
    git_store::Store::open(repo)?.store(AUTH_REF, &auth, "Update authorized signers")?;
    Ok(())
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

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::let_underscore_must_use,
        reason = "unit test"
    )]

    use std::path::PathBuf;
    use std::process::Command;
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
