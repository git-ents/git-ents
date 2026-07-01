//! Fast revocation, sourced from the `refs/meta/revoked` ref.
//!
//! Routine compromise self-heals through expiry: an un-refreshed member key
//! stops authorizing pushes once its window lapses. Revocation is the "faster
//! than expiry" override — a deny list of fingerprints the verifier subtracts
//! from the trust set *before* checking a push, so a compromised key is refused
//! the moment it is listed, without waiting for its window and without editing
//! the member refs that may be governed elsewhere.
//!
//! The list is one ref — `refs/meta/revoked` — precisely so a forge can fan it
//! out to every repository it hosts as a single push (that fan-out is a
//! server-side concern layered on top of this primitive). It denies leaf-key
//! fingerprints; a compromised certificate authority is revoked by removing the
//! CA member itself, since a CA is named by a whole ref rather than listed by
//! fingerprint.
//!
//! # Migration note
//!
//! The on-disk map moved from a bare `String` value to a [`RevocationBody`]
//! struct, dropping the `MapDoc`/`Row` `(String, String)` ceiling. This turns
//! `revoked/<fingerprint>` from a blob into a subtree, an incompatible format
//! change: data written in the prior flat-string layout no longer loads and
//! must be re-recorded. Acceptable pre-1.0 (see the format compatibility
//! rules in `git_store`'s module docs).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use facet::Facet;

/// The ref whose tree holds the revocation list — the deny overlay on the trust
/// set.
pub const REVOKED_REF: &str = "refs/meta/revoked";

/// A revoked key's on-disk body. The map key (its fingerprint) is its
/// identity, so it is not duplicated inside the body.
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
struct RevocationBody {
    /// A free-text reason, or `""` when none was given.
    reason: String,
}

/// The revocation document stored at [`REVOKED_REF`]: its `revoked/` subtree
/// maps each revoked fingerprint to its [`RevocationBody`].
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
struct Revocations {
    revoked: BTreeMap<String, RevocationBody>,
}

/// One revoked key, assembled from its map key and [`RevocationBody`] at load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revocation {
    /// The revoked key's fingerprint — the `members/<fingerprint>` it denies.
    pub fingerprint: String,
    /// A free-text reason, or `""` when none was given.
    pub reason: String,
}

/// Load the revocations recorded at [`REVOKED_REF`] from an already-open
/// `store`. An absent ref yields an empty list — nothing is revoked.
pub fn load_with(store: &git_store::Store) -> Result<Vec<Revocation>, git_store::Error> {
    Ok(store
        .load::<Revocations>(REVOKED_REF)?
        .map(|doc| {
            doc.revoked
                .into_iter()
                .map(|(fingerprint, body)| Revocation {
                    fingerprint,
                    reason: body.reason,
                })
                .collect()
        })
        .unwrap_or_default())
}

/// Load the revocations recorded at [`REVOKED_REF`] in `repo`. See
/// [`load_with`].
pub fn load(repo: &Path) -> Result<Vec<Revocation>, git_store::Error> {
    load_with(&git_store::Store::open(repo)?)
}

/// Write `revocations` to [`REVOKED_REF`] through an already-open `store`,
/// replacing any existing list as a new commit.
pub fn store_with(
    store: &git_store::Store,
    revocations: &[Revocation],
) -> Result<(), git_store::Error> {
    let doc = Revocations {
        revoked: revocations
            .iter()
            .cloned()
            .map(|revocation| {
                (
                    revocation.fingerprint,
                    RevocationBody {
                        reason: revocation.reason,
                    },
                )
            })
            .collect(),
    };
    store.store(REVOKED_REF, &doc, "Update revocations")
}

/// Write `revocations` to [`REVOKED_REF`]. See [`store_with`].
pub fn store(repo: &Path, revocations: &[Revocation]) -> Result<(), git_store::Error> {
    store_with(&git_store::Store::open(repo)?, revocations)
}

/// The set of revoked fingerprints recorded at [`REVOKED_REF`] from an
/// already-open `store`, for the verifier to subtract from the trust set.
pub fn fingerprints_with(store: &git_store::Store) -> Result<BTreeSet<String>, git_store::Error> {
    Ok(load_with(store)?
        .into_iter()
        .map(|revocation| revocation.fingerprint)
        .collect())
}

/// The set of revoked fingerprints recorded at [`REVOKED_REF`] in `repo`. See
/// [`fingerprints_with`].
pub fn fingerprints(repo: &Path) -> Result<BTreeSet<String>, git_store::Error> {
    fingerprints_with(&git_store::Store::open(repo)?)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::let_underscore_must_use,
        reason = "unit test"
    )]

    use super::*;
    use crate::testutil::{unique_repo as new_repo, write_revocations_doc};

    fn unique_repo() -> std::path::PathBuf {
        new_repo("revocations")
    }

    fn revocation(fingerprint: &str, reason: &str) -> Revocation {
        Revocation {
            fingerprint: fingerprint.to_owned(),
            reason: reason.to_owned(),
        }
    }

    #[test]
    fn store_then_load_round_trips_the_revocations() {
        let repo = unique_repo();
        let written = vec![
            revocation("aa:bb", "laptop stolen"),
            revocation("cc:dd", ""),
        ];
        store(&repo, &written).unwrap();
        let mut loaded = load(&repo).unwrap();
        loaded.sort_by(|a, b| a.fingerprint.cmp(&b.fingerprint));
        assert_eq!(loaded, written);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn empty_when_the_revoked_ref_is_absent() {
        let repo = unique_repo();
        assert!(load(&repo).unwrap().is_empty());
        assert!(fingerprints(&repo).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn fingerprints_collects_the_revoked_keys() {
        let repo = unique_repo();
        store(&repo, &[revocation("aa:bb", "x"), revocation("cc:dd", "")]).unwrap();
        assert_eq!(
            fingerprints(&repo).unwrap(),
            BTreeSet::from(["aa:bb".to_owned(), "cc:dd".to_owned()])
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn loads_the_on_disk_revoked_format() {
        // A fixture written as the real `revoked/<fingerprint>/reason` subtree
        // layout must keep loading, guarding the document's shape against an
        // incompatible change to data already on a ref.
        let repo = unique_repo();
        write_revocations_doc(&repo, &[("aa:bb", "laptop stolen"), ("cc:dd", "")]);
        let mut loaded = load(&repo).unwrap();
        loaded.sort_by(|a, b| a.fingerprint.cmp(&b.fingerprint));
        assert_eq!(
            loaded,
            vec![
                revocation("aa:bb", "laptop stolen"),
                revocation("cc:dd", "")
            ]
        );
        let _ = std::fs::remove_dir_all(&repo);
    }
}
