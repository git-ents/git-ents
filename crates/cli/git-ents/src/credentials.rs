//! Per-member BYOK credentials (`roots.config-isolation`): read from the
//! hosted composition root's own deployment config, injected into a
//! sandbox's environment at launch ([`crate::agent_worker::run_agent_exec`],
//! [`crate::plan_worker::run_agent_plan`]) — never repository data
//! (`effect.deployment-property`), never written to any tree this crate
//! builds.
//!
//! # Shape
//!
//! A member's credential is a secret value plus the environment variable
//! name a sandboxed agent command expects it under (an Anthropic API key or
//! subscription token, e.g. `ANTHROPIC_API_KEY`) — every member may use
//! their own credential and their own variable name, since BYOK means each
//! member's runs are billed and rate-limited against their own account.
//!
//! # Where it comes from
//!
//! [`CredentialStore::from_env`] mirrors the two deployment-config
//! conventions this crate already has for the hosted root: an env var
//! naming a *path* the process reads once at startup
//! ([`crate::sign::resolve_key_path`]'s own `user.signingkey`-or-default
//! shape for the worker's signing key), and an env var carrying a
//! deployment secret directly ([`ents_effect::sprite::SPRITES_TOKEN_VAR`],
//! read once by [`ents_effect::sprite::ensure_auth`]). Since one member's
//! credential is itself secret material like the Sprite token, and there
//! may be many members, this store takes the file-path shape:
//! [`CREDENTIALS_FILE_VAR`] names a file the deployment operator provisions
//! (one line per member), read once when a [`crate::root::HostedRoot`]
//! opens and handed down by reference from there — never re-read per run,
//! never read by any code but this composition root
//! (`roots.config-isolation`: "Configuration MUST select trait
//! implementations only at the composition root and MUST NOT leak past
//! it").

use std::collections::HashMap;
use std::path::Path;

use ents_model::MemberId;

use crate::error::{Error, Result};

/// The env var naming the credentials file's path on the hosted deployment.
/// Unset means "no member has a credential" — the common case for a
/// repository with no agent sessions configured at all, and for every
/// non-hosted root ([`crate::root::LocalRoot`] never runs `agent-exec`/
/// `agent-plan` today, so it never constructs a [`CredentialStore`]).
pub const CREDENTIALS_FILE_VAR: &str = "GIT_ENTS_CREDENTIALS_FILE";

/// One member's injected credential: the secret value, and the environment
/// variable name a sandbox's launched command expects it under.
#[derive(Debug, Clone)]
pub struct Credential {
    /// The environment variable name to inject the secret as, e.g.
    /// `ANTHROPIC_API_KEY`.
    pub var: String,
    /// The secret value itself — a member's own Anthropic API key or
    /// subscription token (BYOK).
    pub secret: String,
}

/// Per-member credentials, keyed by [`MemberId`] — deployment state
/// (`effect.deployment-property`), constructed only at a composition root
/// ([`crate::root::HostedRoot::open`]) and handed by reference into
/// [`crate::agent_worker::run_agent_exec`]/[`crate::plan_worker::run_agent_plan`],
/// never persisted to any git object.
#[derive(Debug, Clone, Default)]
pub struct CredentialStore {
    by_member: HashMap<MemberId, Credential>,
}

impl CredentialStore {
    /// An empty store: no member has a credential configured.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Build a store directly from `(member, credential)` pairs — the shape
    /// a test fixture uses; production code reaches a [`CredentialStore`]
    /// only through [`Self::from_env`].
    #[must_use]
    pub fn from_pairs(entries: impl IntoIterator<Item = (MemberId, Credential)>) -> Self {
        Self {
            by_member: entries.into_iter().collect(),
        }
    }

    /// Load from [`CREDENTIALS_FILE_VAR`]'s named file, or [`Self::empty`]
    /// if that env var is unset.
    ///
    /// # Errors
    ///
    /// See [`Self::load`].
    pub fn from_env() -> Result<Self> {
        match std::env::var_os(CREDENTIALS_FILE_VAR) {
            Some(path) => Self::load(Path::new(&path)),
            None => Ok(Self::empty()),
        }
    }

    /// Parse the store from `path`'s contents: one
    /// `<member-id>\t<var-name>\t<secret>` line per member, blank lines and
    /// `#`-prefixed lines ignored.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] if `path` cannot be read; [`Error::InvalidArgument`] for
    /// a line that is not exactly three tab-separated fields, or whose
    /// var-name field is not a POSIX environment variable name
    /// (`[A-Za-z_][A-Za-z0-9_]*`) — the name is folded verbatim into the
    /// `export` statement [`ents_effect::executor::inject_env`] builds for
    /// shell-script backends, so anything else would corrupt the sandbox
    /// script (only the secret *value* is quote-escaped there, on the
    /// grounds that the operator-authored name, unlike the member-supplied
    /// secret, has no reason to ever contain metacharacters).
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|source| Error::Io {
            path: path.to_owned(),
            source,
        })?;
        let mut by_member = HashMap::new();
        for (number, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut fields = line.splitn(3, '\t');
            let (Some(member), Some(var), Some(secret)) =
                (fields.next(), fields.next(), fields.next())
            else {
                return Err(Error::InvalidArgument(format!(
                    "{}:{}: malformed credential line (expected \
                     <member-id>\\t<var-name>\\t<secret>)",
                    path.display(),
                    number.saturating_add(1)
                )));
            };
            if !is_env_var_name(var) {
                return Err(Error::InvalidArgument(format!(
                    "{}:{}: {var:?} is not a valid environment variable name \
                     ([A-Za-z_][A-Za-z0-9_]*)",
                    path.display(),
                    number.saturating_add(1)
                )));
            }
            by_member.insert(
                MemberId::new(member),
                Credential {
                    var: var.to_owned(),
                    secret: secret.to_owned(),
                },
            );
        }
        Ok(Self { by_member })
    }

    /// `member`'s configured credential, if the deployment has one.
    #[must_use]
    pub fn get(&self, member: &MemberId) -> Option<&Credential> {
        self.by_member.get(member)
    }
}

/// Whether `name` is a POSIX environment variable name:
/// `[A-Za-z_][A-Za-z0-9_]*` (see [`CredentialStore::load`]'s own doc for
/// why anything else is refused at parse time).
fn is_env_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "unit test")]

    use rstest::rstest;

    use super::*;

    #[rstest]
    // @relation(roots.config-isolation, scope=function, role=Verifies)
    fn empty_store_has_no_credential_for_anyone() {
        let store = CredentialStore::empty();
        assert!(store.get(&MemberId::new("jdc")).is_none());
    }

    #[rstest]
    // @relation(roots.config-isolation, scope=function, role=Verifies)
    fn from_pairs_looks_up_by_member() {
        let store = CredentialStore::from_pairs([(
            MemberId::new("jdc"),
            Credential {
                var: "ANTHROPIC_API_KEY".to_owned(),
                secret: "sk-ant-abc".to_owned(),
            },
        )]);
        let credential = store.get(&MemberId::new("jdc")).expect("configured");
        assert_eq!(credential.var, "ANTHROPIC_API_KEY");
        assert_eq!(credential.secret, "sk-ant-abc");
        assert!(store.get(&MemberId::new("someone-else")).is_none());
    }

    #[rstest]
    // @relation(roots.config-isolation, scope=function, role=Verifies)
    fn load_parses_one_credential_per_line_and_skips_comments_and_blanks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("credentials");
        std::fs::write(
            &path,
            "# deployment credentials\n\njdc\tANTHROPIC_API_KEY\tsk-ant-abc\n\nmallory\tANTHROPIC_API_KEY\tsk-ant-xyz\n",
        )
        .expect("write");

        let store = CredentialStore::load(&path).expect("parses");
        assert_eq!(
            store.get(&MemberId::new("jdc")).expect("configured").secret,
            "sk-ant-abc"
        );
        assert_eq!(
            store
                .get(&MemberId::new("mallory"))
                .expect("configured")
                .secret,
            "sk-ant-xyz"
        );
    }

    #[rstest]
    // @relation(roots.config-isolation, scope=function, role=Verifies)
    fn load_rejects_a_malformed_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("credentials");
        std::fs::write(&path, "jdc\tANTHROPIC_API_KEY\n").expect("write");

        let error = CredentialStore::load(&path).expect_err("missing the secret field");
        assert!(matches!(error, Error::InvalidArgument(_)));
    }

    #[rstest]
    #[case::shell_metacharacters("KEY; rm -rf /")]
    #[case::leading_digit("1KEY")]
    #[case::empty("")]
    // @relation(roots.config-isolation, scope=function, role=Verifies)
    fn load_rejects_a_var_name_that_is_not_a_posix_env_name(#[case] var: &str) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("credentials");
        std::fs::write(&path, format!("jdc\t{var}\tsk-ant-abc\n")).expect("write");

        let error = CredentialStore::load(&path).expect_err("the var name is folded into shell");
        assert!(matches!(error, Error::InvalidArgument(_)));
    }

    // `from_env`'s own body is a two-arm match over `CREDENTIALS_FILE_VAR`
    // delegating straight to `load` (covered above) or `empty` (covered
    // above); it is deliberately not exercised here via
    // `std::env::set_var`/`remove_var`, since mutating process-global env
    // state is unsound to race against other tests in the same process
    // (`std::env`'s own safety docs) — the two branches it can take are
    // both already proven correct on their own.
}
