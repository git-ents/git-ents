//! The Effect entity: a declarative subscription to a commit-set query.
//!
//! Spec coverage: `model.effect-definition`.

use facet::Facet;

/// A declarative effect definition, living at `refs/meta/effects/<name>`
/// (`namespace::effect_ref`).
///
/// `trigger` is the raw `CommitQuery` text (`query.grammar`): the algebra
/// itself — parsing, footprint extraction, incremental evaluation — is
/// `ents-query`'s domain (phase 3). Storing it as `String` here rather than
/// a parsed AST keeps the dependency edge the crate graph already states:
/// `ents-query` depends on `ents-model`, never the reverse.
///
/// `model.effect-definition` explicitly forbids executor, sandbox, or
/// retry fields — how an effect runs is a deployment property
/// (`effect.deployment-property`), decided by `ents-effect` (phase 5) and
/// the composition root, never stored on the entity itself.
///
/// # Examples
///
/// ```
/// use ents_model::Effect;
///
/// let effect = Effect {
///     trigger: "rev(refs/heads/main)".to_owned(),
///     toolchains: vec!["rust-stable".to_owned()],
///     run: "cargo nextest run".to_owned(),
/// };
/// let (id, store) = facet_git_tree::serialize(&effect).expect("serialize");
/// let back: Effect = facet_git_tree::deserialize(&id, &store).expect("deserialize");
/// assert_eq!(back, effect);
/// ```
// @relation(model.effect-definition, meta-ref.typed-tree, model.extensibility, scope=file)
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct Effect {
    /// The raw `CommitQuery` text denoting the commit set this effect
    /// fires for (`query.grammar`).
    pub trigger: String,
    /// The names of the toolchains this effect's run requires, each a
    /// `refs/meta/toolchains/<name>` reference (`model.toolchain`).
    pub toolchains: Vec<String>,
    /// The run command.
    pub run: String,
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        clippy::panic,
        reason = "unit test; the panic is an assertion the type reflects as a struct at all"
    )]

    use facet::{Facet as _, Type, UserType};
    use facet_git_tree::{deserialize, serialize};
    use rstest::rstest;

    use super::*;

    #[rstest]
    // @relation(model.effect-definition, meta-ref.typed-tree, scope=function, role=Verifies)
    fn effect_round_trips_through_a_tree() {
        let effect = Effect {
            trigger: "rev(refs/heads/main) & results(unit, pass)".to_owned(),
            toolchains: vec!["rust-stable".to_owned(), "node-lts".to_owned()],
            run: "cargo nextest run".to_owned(),
        };
        let (id, store) = serialize(&effect).expect("serialize");
        let back: Effect = deserialize(&id, &store).expect("deserialize");
        assert_eq!(back, effect);
    }

    #[rstest]
    #[case::executor("executor")]
    #[case::sandbox("sandbox")]
    #[case::retry("retry")]
    // @relation(model.effect-definition, scope=function, role=Verifies)
    fn effect_never_carries_a_deployment_field(#[case] forbidden: &str) {
        let Type::User(UserType::Struct(struct_ty)) = Effect::SHAPE.ty else {
            panic!("Effect must reflect as a struct");
        };
        assert!(
            struct_ty.fields.iter().all(|f| f.name != forbidden),
            "Effect must not carry a {forbidden:?} field: how it runs is a deployment property"
        );
    }
}
