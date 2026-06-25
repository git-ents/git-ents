//! The repository's metadata, sourced from the `refs/meta/config` ref.
//!
//! A repository's loose metadata — its description, homepage, and topics — is
//! first-class, members-gated, versioned data rather than worktree content or
//! a loose git file. It lives on exactly one ref, `refs/meta/config`, whose
//! tree is a [`Config`] document read and written through [`git_store`]. Keeping
//! it on a meta ref (not in the worktree) means anyone who can push content
//! cannot rewrite the repository's metadata, and the metadata carries its own
//! independent history.

use std::path::Path;

use facet::Facet;

/// The ref whose tree holds the repository configuration.
pub const CONFIG_REF: &str = "refs/meta/config";

/// The repository configuration stored at [`CONFIG_REF`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Facet)]
pub struct Config {
    /// The repository's description (was git's `.git/description` file).
    pub description: String,
    /// The repository's homepage URL; `""` when unset.
    pub homepage: String,
    /// The repository's topics, members-gated metadata rather than worktree
    /// content.
    pub topics: Vec<String>,
}

/// Load the configuration recorded at [`CONFIG_REF`] in `repo`.
///
/// An absent ref yields [`Config::default`], as on a repository whose metadata
/// has not been set yet. A present but unreadable ref is an error so callers can
/// distinguish corruption from "no configuration set".
pub fn load(repo: &Path) -> Result<Config, git_store::Error> {
    Ok(git_store::Store::open(repo)?
        .load::<Config>(CONFIG_REF)?
        .unwrap_or_default())
}

/// Write `config` to [`CONFIG_REF`], replacing any existing value, as a new
/// commit.
pub fn store(repo: &Path, config: &Config) -> Result<(), git_store::Error> {
    git_store::Store::open(repo)?.store(CONFIG_REF, config, "Update configuration")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::let_underscore_must_use,
        reason = "unit test"
    )]

    use super::*;
    use crate::testutil::{unique_repo as new_repo, write_config_doc};

    fn unique_repo() -> std::path::PathBuf {
        new_repo("config")
    }

    fn config() -> Config {
        Config {
            description: "A repository".to_owned(),
            homepage: "https://example.com".to_owned(),
            topics: vec!["rust".to_owned(), "git".to_owned()],
        }
    }

    #[test]
    fn store_then_load_round_trips_the_config() {
        let repo = unique_repo();
        store(&repo, &config()).unwrap();
        assert_eq!(load(&repo).unwrap(), config());
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn store_replaces_the_previous_config() {
        let repo = unique_repo();
        store(&repo, &config()).unwrap();
        store(&repo, &Config::default()).unwrap();
        assert_eq!(load(&repo).unwrap(), Config::default());
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn loads_the_on_disk_config_format() {
        // A fixture written as the real on-disk layout — `description` and
        // `homepage` blobs plus an index-keyed `topics/` subtree — must keep
        // loading, guarding the Config document's shape against an incompatible
        // change to data already on a ref.
        let repo = unique_repo();
        write_config_doc(
            &repo,
            CONFIG_REF,
            "A repository",
            "https://example.com",
            &["rust", "git"],
        );
        assert_eq!(load(&repo).unwrap(), config());
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn default_when_the_config_ref_is_absent() {
        let repo = unique_repo();
        assert_eq!(load(&repo).unwrap(), Config::default());
        let _ = std::fs::remove_dir_all(&repo);
    }
}
