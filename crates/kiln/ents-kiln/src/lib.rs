//! The toolchain domain: the [`Toolchain`] entity and its
//! resolution/materialization machinery (`model.toolchain`,
//! `effect.toolchains`), plus the `toolchain` porcelain command —
//! kernel-independent, unlike `ents-model`'s remaining entities, because
//! resolving a toolchain needs `ents-effect` (to share its tree-checkout
//! primitive and its `Error`/`Result` type) and `ents-receive` (to propose
//! the import mutation), neither of which a purely declarative vocabulary
//! crate like `ents-model` may depend on.
//!
//! This crate sits *above* the kernel in the dependency graph, not inside
//! it: `ents-model`, `ents-anchor`, `ents-gate`, `ents-query`,
//! `ents-receive`, `ents-effect`, `ents-sync`, and `ents-testutil` must
//! never depend on `ents-kiln` (verified by `grep -rn ents-kiln
//! crates/kernel crates/substrate` finding nothing) — `ents-kiln` depends
//! on them, never the reverse. `git-ents` (the CLI) depends on this crate
//! and mounts its toolchain command through a thin wrapper that only adds
//! signer/actor construction and CLI-facing error rendering, and resolves
//! an effect's declared toolchains through this crate before calling
//! `ents_effect::run::run_effect` (`crate::commands::effect::run` on the
//! CLI side).
//!
//! # Spec coverage
//!
//! From `docs/spec/model.sdoc` and `docs/spec/effect.adoc`:
//!
//! - `model.toolchain` — [`Toolchain`].
//! - `effect.toolchains` — [`Recipe`], [`Component`], [`toolchain::resolve`],
//!   [`toolchain::cache_key`], [`toolchain::materialize`]: reading a
//!   toolchain manifest, parsing its `recipe`, and extracting it to a host
//!   directory. `ents_effect::run::run_one`/`run_effect` no longer resolve
//!   toolchain names themselves — they accept an already-materialized
//!   `&[(String, PathBuf)]` slice; resolving an effect's declared names to
//!   that slice is this crate's job, done by a composition root before it
//!   calls into `ents-effect`'s run loop.
//! - `meta-ref.typed-tree` — `toolchain::entity`'s round-trip test (see
//!   the module itself for the concrete test, folded into `toolchain`'s
//!   private `entity` submodule).
//!
//! # Examples
//!
//! Import a toolchain (embedding a stand-in tree — `ents-kiln`'s own
//! `command::import` walks a real host directory; this example embeds an
//! already-written tree directly to demonstrate just the entity/recipe
//! round trip) and resolve it back.
//!
//! ```
//! use ents_kiln::{Recipe, Toolchain, toolchain};
//! use ents_testutil::{MemRefStore, ObjectStore, write_meta_entity};
//! use gix_object::{Kind, Write as _};
//!
//! let refs = MemRefStore::default();
//! let objects = ObjectStore::default();
//! let bin_tree = objects.write_buf(Kind::Tree, b"").expect("write");
//!
//! let toolchain = Toolchain {
//!     name: "rust-stable".into(),
//!     recipe: Recipe::Embedded { tree: bin_tree }.render(),
//! };
//! let name: gix::refs::FullName = "refs/meta/toolchains/rust-stable".try_into().expect("valid");
//! write_meta_entity(&refs, &objects, name, &toolchain, None, 100);
//!
//! let (entity, recipe) = toolchain::resolve(&refs, &objects, "rust-stable").expect("resolves");
//! assert_eq!(entity.name, "rust-stable");
//!
//! let cache = tempfile::tempdir().expect("tempdir");
//! let bin = toolchain::materialize(&recipe, &objects, cache.path()).expect("materializes");
//! assert!(bin.is_dir());
//! ```

pub mod toolchain;

pub use toolchain::{Component, Recipe, Toolchain, cache_key, materialize, resolve};

#[cfg(test)]
mod tests {
    use facet::Facet as _;
    use rstest::rstest;

    use super::*;

    /// The entity that moved from `ents-model` to this crate keeps the
    /// same `model.extensibility` guarantee `ents_model`'s own shape test
    /// pins for its remaining entities (and `ents-forge`'s own copy pins
    /// for `Comment`/`Issue`): its reflected
    /// [`facet::Shape::type_identifier`] is exactly its Rust struct name.
    #[rstest]
    #[case::toolchain(Toolchain::SHAPE.type_identifier, "Toolchain")]
    // @relation(model.extensibility, scope=function, role=Verifies)
    fn every_entity_shape_name_tracks_its_struct_declaration(
        #[case] reflected: &str,
        #[case] expected: &str,
    ) {
        assert_eq!(reflected, expected);
    }
}
