//! The Toolchain entity: a hash-pinned execution-environment manifest.
//!
//! Spec coverage: `model.toolchain`.

use facet::Facet;

/// A toolchain manifest, living at `refs/meta/toolchains/<name>`
/// (`namespace::toolchain_ref`).
///
/// Content addressing makes the manifest hash-pinned for free: its own
/// tree object id, produced by `facet-git-tree` serialization, already
/// names the exact bytes `recipe` holds. `recipe` carries whatever
/// provenance is needed to reproduce the execution environment; its
/// internal structure (toolchain kind, download vs. embedded binaries,
/// pinned versions) is this crate's own domain — see [`super::resolve`]
/// for the [`super::Recipe`] structure `recipe` parses into.
///
/// # Examples
///
/// ```
/// use ents_kiln::Toolchain;
///
/// let toolchain = Toolchain {
///     name: "rust-stable".to_owned(),
///     recipe: "rustup component add ... pinned to 1.90.0".to_owned(),
/// };
/// let (id, store) = facet_git_tree::serialize(&toolchain).expect("serialize");
/// let back: Toolchain = facet_git_tree::deserialize(&id, &store).expect("deserialize");
/// assert_eq!(back, toolchain);
/// ```
// @relation(model.toolchain, meta-ref.typed-tree, model.extensibility, scope=file)
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct Toolchain {
    /// The toolchain's name — the last segment of its ref.
    pub name: String,
    /// Opaque provenance needed to reproduce the execution environment.
    pub recipe: String,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "unit test")]

    use facet_git_tree::{deserialize, serialize};
    use rstest::rstest;

    use super::*;

    #[rstest]
    // @relation(model.toolchain, meta-ref.typed-tree, scope=function, role=Verifies)
    fn toolchain_round_trips_through_a_tree() {
        let toolchain = Toolchain {
            name: "rust-stable".to_owned(),
            recipe: "recipe text".to_owned(),
        };
        let (id, store) = serialize(&toolchain).expect("serialize");
        let back: Toolchain = deserialize(&id, &store).expect("deserialize");
        assert_eq!(back, toolchain);
    }
}
