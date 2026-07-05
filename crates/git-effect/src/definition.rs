//! The configured effects, sourced from `refs/meta/effects/<name>` — one ref
//! per effect.
//!
//! An effect is anything a server runs against a push — CI, CD, linting,
//! versioning gates, and so on. Decomposed one ref per effect (rather than a
//! single aggregated map, as the prior "checks" naming used) so an effect can
//! be added or removed as an independent, separately-history'd ref, and so the
//! admin-only write rule can be stated as a single refname glob
//! (`refs/meta/effects/*`) instead of gating one shared ref. The document is
//! read and written through [`git_store`], so an effect is a typed value that
//! lives in git — versioned, auditable, and itself pushable. Keeping it on a
//! meta ref rather than in the worktree means an untrusted branch cannot
//! rewrite the effects that gate it.
//!
//! # Migration note
//!
//! Effects were checks: `refs/meta/checks` (one ref, a scalar-keyed map of
//! `checks/<name>` subtrees) decomposed to `refs/meta/effects/<name>` (one ref
//! per effect), and `Check`/`CheckBody` renamed to [`Effect`]/`EffectBody`.
//! Incompatible with data written in the prior layout — acceptable pre-1.0
//! (see the format compatibility rules in `git_store`'s module docs).

use std::path::Path;

use facet::Facet;

use git_store::component;

/// The ref namespace holding the configured effects, one
/// `refs/meta/effects/<name>` ref per effect.
pub const EFFECTS_NS: &str = "refs/meta/effects";

/// The ref holding the effect named `name`.
#[must_use]
pub fn effect_ref(name: &str) -> String {
    format!("{EFFECTS_NS}/{name}")
}

/// A configured effect's on-disk body. The ref's last segment (its name) is
/// the effect's identity, so it is not duplicated inside the body.
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
struct EffectBody {
    /// The shell command run for the effect (e.g. `cargo fmt --check`), or
    /// `None` for a composite effect that only aggregates its `depends`.
    command: Option<String>,
    /// The sandbox image the command runs in; `None` uses the default.
    image: Option<String>,
    /// Names of sibling effects that must pass before this one runs. Stored
    /// as `None` when empty so an independent effect stays a minimal tree.
    depends: Option<Vec<String>>,
    /// Names of toolchains (`git-toolchain`, `refs/meta/toolchains/<name>`)
    /// activated on `PATH` before the command runs. Stored as `None` when
    /// empty, like `depends`.
    toolchains: Option<Vec<String>>,
}

impl component::Collection for EffectBody {
    const NS: &'static str = EFFECTS_NS;
}

/// One configured effect, assembled from its ref name and [`EffectBody`] at
/// load.
///
/// ## Requirements
///
/// @relation(checks.definition)
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct Effect {
    /// The name it is stored under (`refs/meta/effects/<name>`).
    pub name: String,
    /// The shell command run for the effect (e.g. `cargo fmt --check`), or
    /// `None` for a composite effect that only aggregates its dependencies.
    pub command: Option<String>,
    /// The sandbox image the command runs in; `None` uses the default.
    pub image: Option<String>,
    /// Names of sibling effects that must pass before this one runs.
    pub depends: Vec<String>,
    /// Names of toolchains activated on `PATH` before the command runs.
    pub toolchains: Vec<String>,
}

impl component::Component for Effect {
    const NOUN: &'static str = "effect";
    const PLURAL: &'static str = "effects";
}

fn compose(name: String, body: EffectBody) -> Effect {
    Effect {
        name,
        command: body.command,
        image: body.image,
        depends: body.depends.unwrap_or_default(),
        toolchains: body.toolchains.unwrap_or_default(),
    }
}

fn decompose(effect: &Effect) -> EffectBody {
    EffectBody {
        command: effect.command.clone(),
        image: effect.image.clone(),
        depends: if effect.depends.is_empty() {
            None
        } else {
            Some(effect.depends.clone())
        },
        toolchains: if effect.toolchains.is_empty() {
            None
        } else {
            Some(effect.toolchains.clone())
        },
    }
}

/// Load the effect named `name` at [`effect_ref`] in `repo`, or `None` when
/// it is not configured.
pub fn load(repo: &Path, name: &str) -> Result<Option<Effect>, git_store::Error> {
    let store = git_store::Store::open(repo)?;
    Ok(
        component::load_item::<EffectBody>(&store, name)?
            .map(|body| compose(name.to_owned(), body)),
    )
}

/// Load every configured effect in `repo`. An absent [`EFFECTS_NS`] yields an
/// empty set, as on a server whose effects have not been pushed yet.
pub fn load_all(repo: &Path) -> Result<Vec<Effect>, git_store::Error> {
    let store = git_store::Store::open(repo)?;
    Ok(component::list::<EffectBody>(&store)?
        .into_iter()
        .map(|(name, body)| compose(name, body))
        .collect())
}

/// Write `effect` to its own [`effect_ref`] in `repo`, replacing any existing
/// value as a new commit.
pub fn store(repo: &Path, effect: &Effect) -> Result<(), git_store::Error> {
    let store = git_store::Store::open(repo)?;
    component::store_item::<EffectBody>(&store, &effect.name, &decompose(effect), "Update effect")
}

/// Validate `effects` as a static dependency graph and return them in an
/// order that runs every effect after its dependencies — Kahn's topological
/// sort, with ties broken by name so the order is deterministic.
///
/// Rejected here, at write time, so the worker only ever walks a fixed order:
/// a `depends` entry naming no configured effect, a duplicate or self edge, an
/// effect with neither a command nor dependencies, any dependency cycle
/// (reported with its member names), and a `toolchains` entry that is not a
/// valid ref-path segment. An effect that sets an `image` is also rejected
/// until the Sprite sandbox can honor one — the field exists in the format
/// now so supporting it later is not a data migration. Whether a named
/// toolchain actually exists is checked server-side at job time, not here —
/// unlike `depends`, `toolchains` cross-references a different ref
/// namespace this function has no set of configured names to check against.
///
/// ## Requirements
///
/// @relation(checks.definition, checks.toolchains)
pub fn order(effects: &[Effect]) -> Result<Vec<&Effect>, String> {
    let mut by_name: std::collections::BTreeMap<&str, &Effect> = std::collections::BTreeMap::new();
    for effect in effects {
        if by_name.insert(effect.name.as_str(), effect).is_some() {
            return Err(format!("effect {} is defined twice", effect.name));
        }
    }
    let mut blocking: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for effect in effects {
        if effect.command.is_none() && effect.depends.is_empty() {
            return Err(format!(
                "effect {} has neither a command nor dependencies",
                effect.name
            ));
        }
        if effect.image.is_some() {
            return Err(format!(
                "effect {} sets an image, which the effects sandbox does not support yet",
                effect.name
            ));
        }
        for toolchain in &effect.toolchains {
            if !git_store::ref_segment_ok(toolchain) {
                return Err(format!(
                    "effect {} names an invalid toolchain {toolchain:?}",
                    effect.name
                ));
            }
        }
        let mut seen = std::collections::BTreeSet::new();
        for dep in &effect.depends {
            if !by_name.contains_key(dep.as_str()) {
                return Err(format!(
                    "effect {} depends on unknown effect {dep}",
                    effect.name
                ));
            }
            if dep == &effect.name {
                return Err(format!("effect {} depends on itself", effect.name));
            }
            if !seen.insert(dep.as_str()) {
                return Err(format!(
                    "effect {} lists dependency {dep} twice",
                    effect.name
                ));
            }
        }
        blocking.insert(effect.name.as_str(), effect.depends.len());
    }

    let mut ordered = Vec::with_capacity(effects.len());
    while ordered.len() < effects.len() {
        let ready: Vec<&str> = blocking
            .iter()
            .filter_map(|(name, blockers)| (*blockers == 0).then_some(*name))
            .collect();
        if ready.is_empty() {
            let cycle: Vec<&str> = blocking.keys().copied().collect();
            return Err(format!(
                "effect dependencies form a cycle: {}",
                cycle.join(", ")
            ));
        }
        for name in ready {
            let _ready = blocking.remove(name);
            if let Some(effect) = by_name.get(name) {
                ordered.push(*effect);
            }
            for (blocked, blockers) in blocking.iter_mut() {
                if let Some(effect) = by_name.get(blocked)
                    && effect.depends.iter().any(|dep| dep == name)
                {
                    *blockers = blockers.saturating_sub(1);
                }
            }
        }
    }
    Ok(ordered)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::indexing_slicing,
        clippy::let_underscore_must_use,
        reason = "unit test"
    )]

    use super::*;
    use crate::testutil::{unique_repo as new_repo, write_effect_doc};

    fn unique_repo() -> std::path::PathBuf {
        new_repo("effect")
    }

    fn effect(name: &str, command: &str) -> Effect {
        Effect {
            name: name.to_owned(),
            command: Some(command.to_owned()),
            image: None,
            depends: Vec::new(),
            toolchains: Vec::new(),
        }
    }

    fn composite(name: &str, depends: &[&str]) -> Effect {
        Effect {
            name: name.to_owned(),
            command: None,
            image: None,
            depends: depends.iter().map(|dep| (*dep).to_owned()).collect(),
            toolchains: Vec::new(),
        }
    }

    fn dependent(name: &str, command: &str, depends: &[&str]) -> Effect {
        Effect {
            depends: depends.iter().map(|dep| (*dep).to_owned()).collect(),
            ..effect(name, command)
        }
    }

    fn toolchained(name: &str, command: &str, toolchains: &[&str]) -> Effect {
        Effect {
            toolchains: toolchains.iter().map(|t| (*t).to_owned()).collect(),
            ..effect(name, command)
        }
    }

    // @relation(checks.definition, role=Verifies)
    #[test]
    fn store_then_load_round_trips_an_effect() {
        let repo = unique_repo();
        let written = effect("fmt", "cargo fmt --check");
        store(&repo, &written).unwrap();
        assert_eq!(load(&repo, "fmt").unwrap(), Some(written));
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn store_then_load_all_round_trips_the_effect_set() {
        let repo = unique_repo();
        let written = vec![
            effect("fmt", "cargo fmt --check"),
            effect("test", "cargo nextest run"),
        ];
        for item in &written {
            store(&repo, item).unwrap();
        }
        let mut loaded = load_all(&repo).unwrap();
        loaded.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(loaded, written);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn empty_when_no_effects_are_configured() {
        let repo = unique_repo();
        assert!(load_all(&repo).unwrap().is_empty());
        assert!(load(&repo, "fmt").unwrap().is_none());
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn loads_the_on_disk_effect_format() {
        // A fixture written as the real `command/some` subtree layout (the
        // `Option`-wrapped command, with `image`/`depends`/`toolchains`
        // omitted entirely) must keep loading, with the missing optional
        // fields unset — guarding the effect document's shape against an
        // incompatible change to data already on a ref.
        let repo = unique_repo();
        write_effect_doc(&repo, "fmt", "cargo fmt --check");
        assert_eq!(
            load(&repo, "fmt").unwrap(),
            Some(effect("fmt", "cargo fmt --check"))
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    // @relation(checks.definition, role=Verifies)
    #[test]
    fn store_then_load_round_trips_image_and_depends() {
        let repo = unique_repo();
        let written = vec![
            Effect {
                image: Some("rust:1.88".to_owned()),
                ..effect("fmt", "cargo fmt --check")
            },
            dependent("test", "cargo nextest run", &["fmt"]),
            composite("ci", &["fmt", "test"]),
        ];
        for item in &written {
            store(&repo, item).unwrap();
        }
        let mut loaded = load_all(&repo).unwrap();
        loaded.sort_by(|a, b| a.name.cmp(&b.name));
        let mut expected = written;
        expected.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(loaded, expected);
        let _ = std::fs::remove_dir_all(&repo);
    }

    // @relation(checks.definition, role=Verifies)
    #[test]
    fn order_runs_dependencies_first() {
        let effects = vec![
            composite("ci", &["test", "fmt"]),
            dependent("test", "cargo nextest run", &["fmt"]),
            effect("fmt", "cargo fmt --check"),
        ];
        let names: Vec<&str> = order(&effects)
            .unwrap()
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(names, vec!["fmt", "test", "ci"]);
    }

    // @relation(checks.definition, role=Verifies)
    #[test]
    fn order_rejects_a_cycle() {
        let effects = vec![
            dependent("a", "true", &["b"]),
            dependent("b", "true", &["a"]),
            effect("fmt", "cargo fmt --check"),
        ];
        let err = order(&effects).unwrap_err();
        assert!(err.contains("cycle"), "unexpected error: {err}");
        assert!(err.contains('a') && err.contains('b'));
    }

    // @relation(checks.definition, role=Verifies)
    #[test]
    fn order_rejects_an_unknown_dependency() {
        let effects = vec![dependent("test", "cargo nextest run", &["fmt"])];
        let err = order(&effects).unwrap_err();
        assert!(
            err.contains("unknown effect fmt"),
            "unexpected error: {err}"
        );
    }

    // @relation(checks.definition, role=Verifies)
    #[test]
    fn order_rejects_self_and_duplicate_edges() {
        let selfish = vec![dependent("a", "true", &["a"])];
        assert!(order(&selfish).unwrap_err().contains("itself"));
        let doubled = vec![
            effect("fmt", "true"),
            dependent("a", "true", &["fmt", "fmt"]),
        ];
        assert!(order(&doubled).unwrap_err().contains("twice"));
    }

    // @relation(checks.definition, role=Verifies)
    #[test]
    fn order_rejects_an_empty_effect() {
        let effects = vec![composite("hollow", &[])];
        let err = order(&effects).unwrap_err();
        assert!(
            err.contains("neither a command nor dependencies"),
            "unexpected error: {err}"
        );
    }

    // @relation(checks.toolchains, role=Verifies)
    #[test]
    fn order_accepts_a_valid_toolchain_name() {
        let effects = vec![toolchained("build", "make", &["gcc-12"])];
        assert_eq!(
            order(&effects)
                .unwrap()
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            vec!["build"]
        );
    }

    // @relation(checks.toolchains, role=Verifies)
    #[test]
    fn order_rejects_an_invalid_toolchain_name() {
        let effects = vec![toolchained("build", "make", &["not/valid"])];
        let err = order(&effects).unwrap_err();
        assert!(err.contains("invalid toolchain"), "unexpected error: {err}");
    }

    // @relation(checks.toolchains, role=Verifies)
    #[test]
    fn store_then_load_round_trips_toolchains() {
        let repo = unique_repo();
        let written = toolchained("build", "make", &["gcc-12", "cmake"]);
        store(&repo, &written).unwrap();
        assert_eq!(load(&repo, "build").unwrap(), Some(written));
        let _ = std::fs::remove_dir_all(&repo);
    }
}
