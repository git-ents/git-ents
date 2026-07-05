//! Ref-push authorization: whether a member's role permits a push to a given
//! ref, per the rules in `refs/meta/config`.

use git_ents_core::config::Config;

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

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::let_underscore_must_use,
        reason = "unit test"
    )]

    use super::*;
    use git_ents_core::config::RoleRules;

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
