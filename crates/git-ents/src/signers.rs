//! The repository's members, sourced from the `refs/meta/members` ref.
//!
//! Push authentication trusts exactly one place: the `refs/meta/members` ref.
//! Its tree is a [`Members`] document mapping each fingerprint to its OpenSSH
//! public key. A member *is* one or more keys whose signed pushes are accepted.
//! The document is read and written through [`git_store`], so the trust list is
//! a typed value that lives in git — versioned, auditable, and itself pushable.

use std::collections::BTreeMap;
use std::path::Path;

use facet::Facet;

/// The ref whose tree holds the member set — the push trust root.
pub const MEMBERS_REF: &str = "refs/meta/members";

/// The membership document stored at [`MEMBERS_REF`]: its `members/` subtree
/// maps each fingerprint to the OpenSSH public key held there.
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
struct Members {
    members: BTreeMap<String, String>,
}

impl git_store::MapDoc for Members {
    fn from_entries(entries: BTreeMap<String, String>) -> Self {
        Self { members: entries }
    }

    fn into_entries(self) -> BTreeMap<String, String> {
        self.members
    }
}

/// One member's authorized signing key recorded in [`MEMBERS_REF`].
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct Signer {
    /// The key it is stored under — its fingerprint.
    pub fingerprint: String,
    /// The OpenSSH public key the blob holds (`<type> <base64> [comment]`).
    pub key: String,
}

impl git_store::Row for Signer {
    fn from_pair(fingerprint: String, key: String) -> Self {
        Self {
            fingerprint,
            key: key.trim_end().to_owned(),
        }
    }

    fn into_pair(self) -> (String, String) {
        (self.fingerprint, self.key)
    }
}

/// Load the members recorded at [`MEMBERS_REF`] in `repo`.
///
/// An absent ref yields an empty set, as on a fresh server whose trust list has
/// not been pushed yet. A present but unreadable ref is an error so callers can
/// fail closed rather than mistake corruption for "no members".
pub fn load(repo: &Path) -> Result<Vec<Signer>, git_store::Error> {
    git_store::Store::open(repo)?.load_rows::<Members, Signer>(MEMBERS_REF)
}

/// Write `signers` to [`MEMBERS_REF`], replacing any existing set, as a new
/// commit.
pub fn store(repo: &Path, signers: &[Signer]) -> Result<(), git_store::Error> {
    git_store::Store::open(repo)?.store_rows::<Members, _>(
        MEMBERS_REF,
        signers.iter().cloned(),
        "Update members",
    )
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

    use super::*;
    use crate::testutil::{unique_repo as new_repo, write_meta_doc};

    const KEY_A: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaA alice";
    const KEY_B: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbB bob";

    fn unique_repo() -> std::path::PathBuf {
        new_repo("signers")
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
    fn empty_when_the_members_ref_is_absent() {
        let repo = unique_repo();
        assert!(load(&repo).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn loads_the_on_disk_members_format() {
        // A fixture written as the real `members/<fingerprint>` blob layout must
        // keep loading; this fails if the Members document's shape changes
        // incompatibly with data already on a ref.
        let repo = unique_repo();
        write_meta_doc(
            &repo,
            MEMBERS_REF,
            "members",
            &[("aa:bb:cc", KEY_A), ("dd:ee:ff", KEY_B)],
        );
        let mut loaded = load(&repo).unwrap();
        loaded.sort_by(|a, b| a.fingerprint.cmp(&b.fingerprint));
        assert_eq!(
            loaded,
            vec![signer("aa:bb:cc", KEY_A), signer("dd:ee:ff", KEY_B)]
        );
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
