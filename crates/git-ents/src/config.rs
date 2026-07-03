//! The repository's metadata, sourced from the `refs/meta/config` ref.
//!
//! A repository's loose metadata — its description, homepage, and topics — is
//! first-class, members-gated, versioned data rather than worktree content or
//! a loose git file. It lives on exactly one ref, `refs/meta/config`, whose
//! tree is a [`Config`] document read and written through [`git_store`]. Keeping
//! it on a meta ref (not in the worktree) means anyone who can push content
//! cannot rewrite the repository's metadata, and the metadata carries its own
//! independent history.

use std::collections::BTreeMap;
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
    /// Ref-push rules keyed by role name, matched against a pushing
    /// [`crate::members::Member`]'s `role`. A role absent here — or a member
    /// with no role at all — permits every ref: role rules are opt-in gating
    /// layered on top of that default-allow-all rule.
    pub roles: BTreeMap<String, RoleRules>,
}

/// The ref-push rules for one role: glob patterns (`*` matches any run of
/// characters) matched against the full ref name (e.g. `refs/heads/*`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Facet)]
pub struct RoleRules {
    /// Refs this role may push to. Empty means "every ref not denied" —
    /// otherwise a ref must match at least one pattern here.
    pub allow: Vec<String>,
    /// Refs this role may never push to, checked before `allow`.
    pub deny: Vec<String>,
}

/// Whether `role`'s rules in `config` permit pushing to `ref_name`. `role`
/// being `None`, or naming a role absent from `config.roles`, permits every
/// ref — see [`Config::roles`].
#[must_use]
pub fn ref_allowed(config: &Config, role: Option<&str>, ref_name: &str) -> bool {
    let Some(role) = role else {
        return true;
    };
    let Some(rules) = config.roles.get(role) else {
        return true;
    };
    if rules
        .deny
        .iter()
        .any(|pattern| glob_match(pattern, ref_name))
    {
        return false;
    }
    rules.allow.is_empty()
        || rules
            .allow
            .iter()
            .any(|pattern| glob_match(pattern, ref_name))
}

/// Whether `text` matches `pattern`, where `*` in `pattern` matches any run of
/// characters (including none, and including `/`).
#[must_use]
pub fn glob_match(pattern: &str, text: &str) -> bool {
    fn go(pattern: &[u8], text: &[u8]) -> bool {
        match pattern.split_first() {
            None => text.is_empty(),
            Some((b'*', rest)) => {
                go(rest, text)
                    || match text.split_first() {
                        Some((_, t_rest)) => go(pattern, t_rest),
                        None => false,
                    }
            }
            Some((c, rest)) => match text.split_first() {
                Some((t, t_rest)) if t == c => go(rest, t_rest),
                _ => false,
            },
        }
    }
    go(pattern.as_bytes(), text.as_bytes())
}

/// Load the configuration recorded at [`CONFIG_REF`] from an already-open
/// `store`.
///
/// An absent ref yields [`Config::default`], as on a repository whose metadata
/// has not been set yet. A present but unreadable ref is an error so callers can
/// distinguish corruption from "no configuration set".
pub fn load_with(store: &git_store::Store) -> Result<Config, git_store::Error> {
    Ok(store.load::<Config>(CONFIG_REF)?.unwrap_or_default())
}

/// Load the configuration recorded at [`CONFIG_REF`] in `repo`. See
/// [`load_with`].
pub fn load(repo: &Path) -> Result<Config, git_store::Error> {
    load_with(&git_store::Store::open(repo)?)
}

/// Write `config` to [`CONFIG_REF`] in `repo`, replacing any existing value as
/// a new commit.
///
/// The web write path does not call this directly: it lands an edit through
/// `git_ents_server::web::write::signed_edit`, which stages the commit on a
/// throwaway ref and pushes it onto [`CONFIG_REF`] through a signed push, so
/// the `pre-receive` gate judges the change rather than a direct write.
pub fn store(repo: &Path, config: &Config) -> Result<(), git_store::Error> {
    git_store::Store::open(repo)?.store(CONFIG_REF, config, "Update configuration")
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
            roles: BTreeMap::new(),
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

    #[test]
    fn glob_match_supports_a_trailing_star() {
        assert!(glob_match("refs/heads/*", "refs/heads/main"));
        assert!(glob_match("refs/heads/*", "refs/heads/"));
        assert!(!glob_match("refs/heads/*", "refs/tags/v1"));
        assert!(glob_match("*", "anything"));
    }

    #[test]
    fn ref_allowed_defaults_to_true_with_no_role_or_unlisted_role() {
        let mut config = Config::default();
        config.roles.insert(
            "readonly".to_owned(),
            RoleRules {
                allow: vec![],
                deny: vec!["refs/heads/*".to_owned()],
            },
        );
        assert!(ref_allowed(&config, None, "refs/heads/main"));
        assert!(ref_allowed(&config, Some("nonexistent"), "refs/heads/main"));
    }

    #[test]
    fn ref_allowed_checks_deny_before_allow() {
        let mut config = Config::default();
        config.roles.insert(
            "release-manager".to_owned(),
            RoleRules {
                allow: vec!["refs/heads/release-*".to_owned()],
                deny: vec!["refs/heads/release-locked".to_owned()],
            },
        );
        assert!(ref_allowed(
            &config,
            Some("release-manager"),
            "refs/heads/release-1.0"
        ));
        assert!(!ref_allowed(
            &config,
            Some("release-manager"),
            "refs/heads/release-locked"
        ));
        assert!(!ref_allowed(
            &config,
            Some("release-manager"),
            "refs/heads/main"
        ));
    }
}
