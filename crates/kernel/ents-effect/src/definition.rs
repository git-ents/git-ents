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
///     name: "unit".into(),
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

/// The canonical `agent-exec` effect's own name
/// (`docs/agent-sessions-plan.adoc`'s Phase 2) — the final segment of
/// `refs/meta/effects/agent-exec` (`model.effect-definition`).
pub const AGENT_EXEC_NAME: &str = "agent-exec";

/// `agent-exec`'s trigger: every author-written `refs/meta/agent-sessions/*`
/// tip — every commit entering the agent-sessions namespace
/// (`docs/agent-sessions-plan.adoc`'s Phase 2, "An `agent-exec` effect
/// subscribed via `meta(...)` to the agent namespace"). `meta()`'s own
/// grammar rule (`query.meta`) only forbids matching an effect-written
/// namespace — `refs/meta/results/*` or `refs/meta/index/*` — and
/// `refs/meta/agent-sessions/*` is neither, so this glob needs no
/// grammar extension (`query.no-extensions`); this module's own tests pin
/// that against the real parser.
pub const AGENT_EXEC_TRIGGER: &str = "meta(refs/meta/agent-sessions/*)";

/// The canonical `agent-exec` [`Effect`] definition
/// (`docs/agent-sessions-plan.adoc`'s Phase 2): fires once per commit
/// entering the agent-sessions namespace. `toolchains` and `run` are a
/// deployment's own choice — this constructor only fixes the two fields
/// that make the effect *this* effect, `name` and `trigger`
/// (`model.effect-definition`); a real deployment still writes its own
/// signed commit onto `refs/meta/effects/agent-exec` through the ordinary
/// admin-only path (`effect.admin-only`), this fixture is not that write.
///
/// # Examples
///
/// ```
/// use ents_effect::definition::{agent_exec, validate};
///
/// let effect = agent_exec(vec!["agent-runtime".to_owned()], "git-ents agent-exec run");
/// assert_eq!(effect.name, "agent-exec");
/// validate(&effect).expect("the canonical trigger validates");
/// ```
#[must_use]
pub fn agent_exec(toolchains: Vec<String>, run: impl Into<String>) -> Effect {
    Effect {
        name: AGENT_EXEC_NAME.to_owned(),
        trigger: AGENT_EXEC_TRIGGER.to_owned(),
        toolchains,
        run: run.into(),
    }
}

/// The canonical `agent-plan` effect's own name
/// (`docs/agent-sessions-plan.adoc`'s Phase 4, "headless plan drafting is a
/// second effect (`agent-plan`)") — the final segment of
/// `refs/meta/effects/agent-plan`.
pub const AGENT_PLAN_NAME: &str = "agent-plan";

/// `agent-plan`'s trigger: identical to [`AGENT_EXEC_TRIGGER`] — every
/// author-written `refs/meta/agent-sessions/*` tip. Both effects share one
/// query grammar; the plan's own words are "the runner decides by
/// inspecting the tip" — what tells `agent-plan` and `agent-exec` apart is
/// never the trigger, only each effect's own dispatch predicate
/// (`ents_forge::agent::dispatch_plan` here, `ents_forge::agent::dispatch`
/// for `agent-exec`) and its own results namespace
/// (`refs/meta/results/agent-plan/*`, distinct from `agent-exec`'s).
pub const AGENT_PLAN_TRIGGER: &str = AGENT_EXEC_TRIGGER;

/// The canonical `agent-plan` [`Effect`] definition
/// (`docs/agent-sessions-plan.adoc`'s Phase 4): headless plan drafting,
/// firing on every commit entering the agent-sessions namespace exactly
/// like `agent-exec` — the runner's own dispatch predicate is what makes it
/// a cheap no-op except when a session is `planning`, carries a prompt,
/// and has no plan leaf yet. `toolchains` and `run` are a deployment's own
/// choice, exactly as [`agent_exec`]'s own doc explains for its two fixed
/// fields.
///
/// # Examples
///
/// ```
/// use ents_effect::definition::{agent_plan, validate};
///
/// let effect = agent_plan(vec!["agent-runtime".to_owned()], "git-ents agent-plan draft");
/// assert_eq!(effect.name, "agent-plan");
/// validate(&effect).expect("the canonical trigger validates");
/// ```
#[must_use]
pub fn agent_plan(toolchains: Vec<String>, run: impl Into<String>) -> Effect {
    Effect {
        name: AGENT_PLAN_NAME.to_owned(),
        trigger: AGENT_PLAN_TRIGGER.to_owned(),
        toolchains,
        run: run.into(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "unit test")]

    use rstest::rstest;

    use super::*;

    fn effect(trigger: &str, toolchains: &[&str]) -> Effect {
        Effect {
            name: "unit".to_owned(),
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

    // ---- The canonical `agent-exec` definition
    // (`docs/agent-sessions-plan.adoc`'s Phase 2) ----

    #[rstest]
    // @relation(query.grammar, scope=function, role=Verifies)
    fn agent_exec_trigger_parses_against_the_real_query_grammar() {
        AGENT_EXEC_TRIGGER
            .parse::<ents_query::Query>()
            .expect("the canonical agent-exec trigger parses");
    }

    #[rstest]
    // @relation(effect.validation, query.meta, scope=function, role=Verifies)
    fn agent_exec_definition_validates() {
        let effect = agent_exec(vec!["agent-runtime".to_owned()], "git-ents agent-exec run");
        assert_eq!(effect.name, AGENT_EXEC_NAME);
        validate(&effect).expect("the canonical agent-exec definition validates");
    }

    /// `query.meta` forbids `meta(glob)` from matching only
    /// `refs/meta/results/*` and `refs/meta/index/*` — the agent-sessions
    /// namespace is neither, so it must never be rejected as
    /// effect-written the way `validate_rejects_a_meta_glob_naming_an_effect_written_namespace`
    /// proves the results namespace is.
    #[rstest]
    // @relation(query.meta, scope=function, role=Verifies)
    fn the_agent_sessions_namespace_is_not_rejected_as_effect_written() {
        assert!(
            validate(&effect(AGENT_EXEC_TRIGGER, &[])).is_ok(),
            "refs/meta/agent-sessions/* is an author-written namespace, not one of \
             query.meta's forbidden effect-written namespaces (refs/meta/results/*, \
             refs/meta/index/*)"
        );
    }

    // ---- The canonical `agent-plan` definition
    // (`docs/agent-sessions-plan.adoc`'s Phase 4) ----

    #[rstest]
    // @relation(query.grammar, scope=function, role=Verifies)
    fn agent_plan_trigger_parses_against_the_real_query_grammar() {
        AGENT_PLAN_TRIGGER
            .parse::<ents_query::Query>()
            .expect("the canonical agent-plan trigger parses");
    }

    #[rstest]
    // @relation(effect.validation, query.meta, scope=function, role=Verifies)
    fn agent_plan_definition_validates() {
        let effect = agent_plan(
            vec!["agent-runtime".to_owned()],
            "git-ents agent-plan draft",
        );
        assert_eq!(effect.name, AGENT_PLAN_NAME);
        validate(&effect).expect("the canonical agent-plan definition validates");
    }

    #[rstest]
    // @relation(scope=function, role=Verifies)
    fn agent_plan_and_agent_exec_share_a_trigger_but_not_a_name() {
        assert_eq!(AGENT_PLAN_TRIGGER, AGENT_EXEC_TRIGGER);
        assert_ne!(AGENT_PLAN_NAME, AGENT_EXEC_NAME);
    }
}
