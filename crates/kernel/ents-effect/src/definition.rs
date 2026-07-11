//! Write-time validation of an effect definition (`effect.validation`).
//!
//! `ents-receive` cannot call this (`arch.query-effect-split`: no push path
//! may link executor code), so a future frontend that builds an effect
//! definition's commit (`git effect add`, `git-ents` bin, phase 6) calls
//! [`validate`] itself before ever proposing the write — `receive` still
//! admits or refuses the *push* on its own terms (the gate, `receive.unit`);
//! this only keeps a frontend from proposing a definition that could never
//! usefully run.

use ents_model::{Effect, namespace};
use ents_query::Query;

use crate::error::{Error, Result};

/// Reject `effect` before it is ever proposed to `receive`, per
/// `effect.validation`: every name in `toolchains` must be a valid
/// ref-path segment, and `trigger` must parse as a `CommitQuery`
/// (`query.grammar`) — which already rejects a `rev(expr)` naming a
/// `refs/meta/*` pattern (`query.rev`) and a `meta(glob)` naming an
/// effect-written namespace (`query.meta`), since the parser enforces
/// both.
///
/// # Errors
///
/// [`Error::Trigger`] if `trigger` does not parse; [`Error::InvalidToolchainName`]
/// for the first toolchain name that is not a valid ref-path segment.
///
/// # Examples
///
/// ```
/// use ents_effect::definition::validate;
/// use ents_model::Effect;
///
/// let good = Effect {
///     trigger: "rev(refs/heads/main)".into(),
///     toolchains: vec!["rust-stable".into()],
///     run: "cargo test".into(),
/// };
/// assert!(validate(&good).is_ok());
///
/// let bad_trigger = Effect { trigger: "not a query".into(), ..good.clone() };
/// assert!(validate(&bad_trigger).is_err());
///
/// let bad_toolchain = Effect { toolchains: vec!["../escape".into()], ..good };
/// assert!(validate(&bad_toolchain).is_err());
/// ```
// @relation(effect.validation, scope=function)
pub fn validate(effect: &Effect) -> Result<()> {
    effect.trigger.parse::<Query>().map_err(Error::from)?;
    for name in &effect.toolchains {
        namespace::toolchain_ref(name)
            .map_err(|_invalid| Error::InvalidToolchainName(name.clone()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "unit test")]

    use rstest::rstest;

    use super::*;

    fn effect(trigger: &str, toolchains: &[&str]) -> Effect {
        Effect {
            trigger: trigger.to_owned(),
            toolchains: toolchains.iter().map(|s| (*s).to_owned()).collect(),
            run: "true".to_owned(),
        }
    }

    #[rstest]
    // @relation(effect.validation, scope=function, role=Verifies)
    fn validate_accepts_a_well_formed_definition() {
        validate(&effect("rev(refs/heads/main)", &["rust-stable"])).expect("well-formed");
    }

    #[rstest]
    // @relation(effect.validation, scope=function, role=Verifies)
    fn validate_rejects_an_unparsable_trigger() {
        assert!(validate(&effect("not a query", &[])).is_err());
    }

    #[rstest]
    // @relation(effect.validation, query.rev, scope=function, role=Verifies)
    fn validate_rejects_a_rev_naming_a_meta_pattern() {
        assert!(validate(&effect("rev(refs/meta/effects/*)", &[])).is_err());
    }

    #[rstest]
    // @relation(effect.validation, query.meta, scope=function, role=Verifies)
    fn validate_rejects_a_meta_glob_naming_an_effect_written_namespace() {
        assert!(validate(&effect("meta(refs/meta/results/*)", &[])).is_err());
    }

    #[rstest]
    // @relation(effect.validation, scope=function, role=Verifies)
    fn validate_rejects_an_invalid_toolchain_name() {
        assert!(validate(&effect("rev(refs/heads/main)", &["../escape"])).is_err());
    }
}
