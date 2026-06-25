//! The repository's members, sourced from the `refs/meta/members` ref.
//!
//! Push authentication trusts exactly one place: the `refs/meta/members` ref.
//! Its tree is a [`Members`] document mapping each fingerprint to the OpenSSH
//! public key held there and the validity window it is trusted within. A member
//! *is* one or more keys whose signed pushes are accepted while in window. The
//! document is read and written through [`git_store`], so the trust list is a
//! typed value that lives in git — versioned, auditable, and itself pushable.
//!
//! # Expiry
//!
//! Each key carries an optional `valid-after`/`valid-before` window rendered
//! into the `allowed_signers` file git verifies pushes against. The window is
//! the security primitive: an un-refreshed key stops authorizing *new* pushes
//! once it lapses, so stale trust fails closed. A previously-valid push stays
//! verifiable forever, since `ssh-keygen -Y verify -Overify-time` can pin the
//! check to the time the push was made.

use std::collections::BTreeMap;
use std::path::Path;

use facet::Facet;

/// The ref whose tree holds the member set — the push trust root.
pub const MEMBERS_REF: &str = "refs/meta/members";

/// The membership document stored at [`MEMBERS_REF`]: its `members/` subtree
/// maps each fingerprint to the [`Authorization`] held under it.
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
struct Members {
    members: BTreeMap<String, Authorization>,
}

/// One fingerprint's stored authorization: the OpenSSH public key it names and
/// the window that key is trusted within. The fingerprint is the map key, so it
/// is not repeated here.
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
struct Authorization {
    /// The OpenSSH public key the member signs with (`<type> <base64> [comment]`).
    key: String,
    /// The key is trusted at or after this OpenSSH timestamp; `None` is no lower
    /// bound.
    valid_after: Option<String>,
    /// The key is trusted at or before this OpenSSH timestamp; `None` is no upper
    /// bound — trust that never lapses on its own.
    valid_before: Option<String>,
}

/// One member's authorized signing key recorded in [`MEMBERS_REF`], with the
/// validity window it is trusted within.
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct Signer {
    /// The key it is stored under — its fingerprint.
    pub fingerprint: String,
    /// The OpenSSH public key the blob holds (`<type> <base64> [comment]`).
    pub key: String,
    /// The OpenSSH timestamp (`YYYYMMDD[Z]` or `YYYYMMDDHHMM[SS][Z]`) at or after
    /// which the key is trusted, or `None` for no lower bound.
    pub valid_after: Option<String>,
    /// The OpenSSH timestamp at or before which the key is trusted, or `None` for
    /// trust that never lapses on its own.
    pub valid_before: Option<String>,
}

/// Load the members recorded at [`MEMBERS_REF`] in `repo`.
///
/// An absent ref yields an empty set, as on a fresh server whose trust list has
/// not been pushed yet. A present but unreadable ref is an error so callers can
/// fail closed rather than mistake corruption for "no members".
pub fn load(repo: &Path) -> Result<Vec<Signer>, git_store::Error> {
    let Some(doc) = git_store::Store::open(repo)?.load::<Members>(MEMBERS_REF)? else {
        return Ok(Vec::new());
    };
    Ok(doc
        .members
        .into_iter()
        .map(|(fingerprint, held)| Signer {
            fingerprint,
            key: held.key,
            valid_after: held.valid_after,
            valid_before: held.valid_before,
        })
        .collect())
}

/// Write `signers` to [`MEMBERS_REF`], replacing any existing set, as a new
/// commit.
pub fn store(repo: &Path, signers: &[Signer]) -> Result<(), git_store::Error> {
    let members = Members {
        members: signers
            .iter()
            .map(|signer| {
                (
                    signer.fingerprint.clone(),
                    Authorization {
                        key: signer.key.clone(),
                        valid_after: signer.valid_after.clone(),
                        valid_before: signer.valid_before.clone(),
                    },
                )
            })
            .collect(),
    };
    git_store::Store::open(repo)?.store(MEMBERS_REF, &members, "Update members")?;
    Ok(())
}

/// Render `signers` as an OpenSSH `allowed_signers` file that authorizes any
/// pusher identity (`*`) signing in git's namespace.
///
/// The principal is a wildcard because authentication here is membership of the
/// key set, not a binding between a key and a particular identity: `ssh-keygen
/// -Y verify` accepts the push certificate as long as the signing key is one of
/// these, whatever name the pusher signed under.
///
/// Each key's `valid-after`/`valid-before` window is rendered as `allowed_signers`
/// options so git enforces expiry: out-of-window keys are not accepted. Options
/// are comma-joined, the syntax OpenSSH requires for more than one.
#[must_use]
pub fn allowed_signers(signers: &[Signer]) -> String {
    signers.iter().map(allowed_signers_line).collect()
}

/// One `allowed_signers` line for `signer`: the wildcard principal, its validity
/// window, the git namespace, and the key.
fn allowed_signers_line(signer: &Signer) -> String {
    let mut options = Vec::new();
    if let Some(after) = &signer.valid_after {
        options.push(format!("valid-after=\"{after}\""));
    }
    if let Some(before) = &signer.valid_before {
        options.push(format!("valid-before=\"{before}\""));
    }
    options.push("namespaces=\"git\"".to_owned());
    format!("* {} {}\n", options.join(","), signer.key)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::let_underscore_must_use,
        reason = "unit test"
    )]

    use super::*;
    use crate::testutil::{unique_repo as new_repo, write_members_doc};

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
            valid_after: None,
            valid_before: None,
        }
    }

    #[test]
    fn store_then_load_round_trips_the_signer_set() {
        let repo = unique_repo();
        let mut bounded = signer("SHA256-bbb", KEY_B);
        bounded.valid_after = Some("20260101".to_owned());
        bounded.valid_before = Some("20270101".to_owned());
        let written = vec![signer("SHA256-aaa", KEY_A), bounded];
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
        // A fixture written as the real `members/<fingerprint>/{key,valid_after,
        // valid_before}` layout must keep loading; this fails if the Members
        // document's shape changes incompatibly with data already on a ref.
        let repo = unique_repo();
        write_members_doc(
            &repo,
            MEMBERS_REF,
            &[
                ("aa:bb:cc", KEY_A, None, None),
                ("dd:ee:ff", KEY_B, None, Some("20270101")),
            ],
        );
        let mut loaded = load(&repo).unwrap();
        loaded.sort_by(|a, b| a.fingerprint.cmp(&b.fingerprint));
        let mut expected_b = signer("dd:ee:ff", KEY_B);
        expected_b.valid_before = Some("20270101".to_owned());
        assert_eq!(loaded, vec![signer("aa:bb:cc", KEY_A), expected_b]);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn renders_a_wildcard_allowed_signers_file() {
        assert_eq!(
            allowed_signers(&[signer("SHA256-aaa", KEY_A)]),
            format!("* namespaces=\"git\" {KEY_A}\n")
        );
    }

    #[test]
    fn renders_the_validity_window_as_comma_joined_options() {
        let mut bounded = signer("SHA256-aaa", KEY_A);
        bounded.valid_after = Some("20260101".to_owned());
        bounded.valid_before = Some("20270101".to_owned());
        assert_eq!(
            allowed_signers(&[bounded]),
            format!(
                "* valid-after=\"20260101\",valid-before=\"20270101\",namespaces=\"git\" {KEY_A}\n"
            )
        );
    }
}
