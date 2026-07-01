//! The repository's account identity, sourced from the `refs/meta/account` ref.
//!
//! An *account* is just a repository that carries a `refs/meta/account` ref:
//! identity is a repo, not a row in a central table. By convention an account
//! repo is named `user/<username>`, but the trust never rests on that path —
//! move the repo, keep the identity. The presence of the ref is what marks a
//! repository as an account; its [`Account`] document carries the profile. This
//! is the did:web-shaped identity the member refs will eventually `@`-mention.

use std::path::Path;

use facet::Facet;

/// The ref whose tree holds the account profile, and whose mere presence marks a
/// repository as an account repo.
pub const ACCOUNT_REF: &str = "refs/meta/account";

/// A repository's account profile, stored at [`ACCOUNT_REF`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Facet)]
pub struct Account {
    /// The account's username — by convention the `user/<username>` repo name,
    /// but authoritative here rather than in the path.
    pub username: String,
    /// The human-facing display name; defaults to the username.
    pub display_name: String,
    /// A short free-text bio; `""` when unset.
    pub bio: String,
    /// When the account was created, as seconds since the Unix epoch.
    pub created_at: u64,
}

/// Load the account profile at [`ACCOUNT_REF`] in `repo`, or `None` when the
/// ref is absent — i.e. when the repository is not an account repo.
pub fn load(repo: &Path) -> Result<Option<Account>, git_store::Error> {
    git_store::Store::open(repo)?.load::<Account>(ACCOUNT_REF)
}

/// Write `account` to [`ACCOUNT_REF`] in `repo`, replacing any existing value
/// as a new commit.
pub fn store(repo: &Path, account: &Account) -> Result<(), git_store::Error> {
    git_store::Store::open(repo)?.store(ACCOUNT_REF, account, "Update account")
}

/// Whether `repo` is an account repo — whether it carries [`ACCOUNT_REF`].
pub fn is_account_repo(repo: &Path) -> Result<bool, git_store::Error> {
    Ok(load(repo)?.is_some())
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::let_underscore_must_use,
        reason = "unit test"
    )]

    use super::*;
    use crate::testutil::{unique_repo as new_repo, write_account_doc};

    fn unique_repo() -> std::path::PathBuf {
        new_repo("account")
    }

    fn account() -> Account {
        Account {
            username: "alice".to_owned(),
            display_name: "Alice".to_owned(),
            bio: "builder of trees".to_owned(),
            created_at: 1_700_000_000,
        }
    }

    #[test]
    fn store_then_load_round_trips_the_account() {
        let repo = unique_repo();
        store(&repo, &account()).unwrap();
        assert_eq!(load(&repo).unwrap(), Some(account()));
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn absent_account_ref_is_not_an_account_repo() {
        let repo = unique_repo();
        assert_eq!(load(&repo).unwrap(), None);
        assert!(!is_account_repo(&repo).unwrap());
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn the_account_ref_marks_an_account_repo() {
        let repo = unique_repo();
        store(&repo, &account()).unwrap();
        assert!(is_account_repo(&repo).unwrap());
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn loads_the_on_disk_account_format() {
        // A fixture written as the real on-disk layout — `username`,
        // `display_name`, `bio`, and `created_at` blobs — must keep loading,
        // guarding the Account document's shape against an incompatible change to
        // data already on a ref.
        let repo = unique_repo();
        write_account_doc(&repo, "alice", "Alice", "builder of trees", 1_700_000_000);
        assert_eq!(load(&repo).unwrap(), Some(account()));
        let _ = std::fs::remove_dir_all(&repo);
    }
}
