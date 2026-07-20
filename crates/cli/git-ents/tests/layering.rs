//! Mechanical check of the one-way crate layering documented in
//! `docs/abstractions.adoc`'s "Layering" section: substrate -> kernel ->
//! {forge, kiln} -> cli, with forge and kiln forbidden from depending on
//! each other. Extended with one more rule for `crates/verify/*`
//! (`verify/README.adoc`): that layer is a pure sink above everything
//! else — `ents-verify` may depend only on `ents-gate-rules`, and no
//! crate anywhere in the workspace may depend on `ents-verify`.
//!
//! The prose in `docs/abstractions.adoc` states the rule, but nothing
//! stops a future `Cargo.toml` edit from quietly violating it (a kernel
//! crate reaching for `ents-forge` behind a feature flag, say). This test
//! reads the real dependency graph via `cargo metadata`, assigns every
//! workspace member a layer from its manifest path, and asserts each
//! workspace-local dependency edge points from an equal-or-higher layer
//! down to a lower-or-equal one — catching the violation mechanically
//! instead of relying on review to notice.
#![allow(clippy::expect_used, reason = "integration test")]
#![allow(
    clippy::panic,
    reason = "this test's whole job is to panic loudly on an unexpected manifest shape or a layering violation"
)]

use std::collections::BTreeMap;

use cargo_metadata::camino::Utf8Path;
use cargo_metadata::{MetadataCommand, Package};

/// Layer rank for a workspace package, derived from the `crates/<layer>/*`
/// prefix of its manifest path: substrate (0) -> kernel (1) ->
/// forge/kiln (2) -> cli (3) -> verify (4).
///
/// A dependency edge is only legal when the depending package's rank is
/// greater than or equal to the depended-on package's rank — e.g. the CLI
/// (3) may depend on `ents-forge` (2), but a kernel crate (1) may never
/// depend on a package crate (2). `verify` sits at the top rank alone, so
/// this general rule already forbids every other layer from depending on
/// it; [`ents_verify_depends_only_on_ents_gate_rules`] adds the sharper
/// rule that its own outgoing edges are restricted too.
fn layer_rank(package: &Package, workspace_root: &Utf8Path) -> u8 {
    let relative = package
        .manifest_path
        .strip_prefix(workspace_root)
        .unwrap_or_else(|_| {
            panic!(
                "{}'s manifest path {} is not under the workspace root {workspace_root}",
                package.name, package.manifest_path
            )
        });
    let mut components = relative.components();
    assert_eq!(
        components.next().map(|c| c.as_str()),
        Some("crates"),
        "{}'s manifest path {relative} is not under crates/",
        package.name
    );
    match components.next().map(|c| c.as_str()) {
        Some("substrate") => 0,
        Some("kernel") => 1,
        Some("forge") | Some("kiln") => 2,
        Some("cli") => 3,
        Some("verify") => 4,
        other => panic!(
            "{}'s manifest path {relative} has an unrecognized layer directory {other:?}",
            package.name
        ),
    }
}

/// Every workspace-local dependency edge points from a higher (or equal)
/// layer to a lower (or equal) one, and `ents-forge`/`ents-kiln` never
/// depend on each other.
///
/// See `crates/kernel/ents-model` and the root `Cargo.toml` for the
/// self-verification this test is designed to catch: temporarily adding
/// `ents-forge` as a dependency of a kernel crate must fail this test.
#[test]
fn dependencies_never_point_upward() {
    let metadata = MetadataCommand::new()
        .manifest_path(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
        .no_deps()
        .exec()
        .expect("cargo metadata");

    let workspace_packages = metadata.workspace_packages();

    let ranks: BTreeMap<&str, u8> = workspace_packages
        .iter()
        .map(|package| {
            (
                package.name.as_ref(),
                layer_rank(package, &metadata.workspace_root),
            )
        })
        .collect();

    let mut forge_depends_on_kiln = false;
    let mut kiln_depends_on_forge = false;

    for package in &workspace_packages {
        let from_rank = *ranks
            .get(package.name.as_ref())
            .expect("every workspace package has a rank");
        for dependency in &package.dependencies {
            let Some(&to_rank) = ranks.get(dependency.name.as_str()) else {
                continue; // external crate, not workspace-local
            };
            assert!(
                from_rank >= to_rank,
                "layering violation: {} (layer {from_rank}) depends on {} (layer {to_rank}), \
                 but dependencies must point from a higher layer down to an equal-or-lower one",
                package.name,
                dependency.name,
            );

            if package.name == "ents-forge" && dependency.name == "ents-kiln" {
                forge_depends_on_kiln = true;
            }
            if package.name == "ents-kiln" && dependency.name == "ents-forge" {
                kiln_depends_on_forge = true;
            }

            if package.name == "ents-verify" {
                assert_eq!(
                    dependency.name, "ents-gate-rules",
                    "ents-verify (the verify/ layer's sink crate) may depend on ents-gate-rules only, \
                     but its manifest also names {}",
                    dependency.name
                );
            }
        }
    }

    assert!(
        !forge_depends_on_kiln,
        "ents-forge must not depend on ents-kiln: they are sibling package crates"
    );
    assert!(
        !kiln_depends_on_forge,
        "ents-kiln must not depend on ents-forge: they are sibling package crates"
    );
}

/// `ents-verify` is a pure sink: nothing in the workspace may depend on
/// it. The general rank rule in [`dependencies_never_point_upward`]
/// already forbids this (every other layer ranks below `verify`), but
/// this test states the sink property directly against the real
/// dependency graph, rather than relying on that rank arithmetic alone.
///
/// See `crates/kernel/ents-model` and the root `Cargo.toml` for the
/// self-verification this test is designed to catch: temporarily adding
/// `ents-verify` as a dependency of any other crate must fail this test.
#[test]
fn nothing_depends_on_ents_verify() {
    let metadata = MetadataCommand::new()
        .manifest_path(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
        .no_deps()
        .exec()
        .expect("cargo metadata");

    for package in metadata.workspace_packages() {
        if package.name == "ents-verify" {
            continue;
        }
        assert!(
            package
                .dependencies
                .iter()
                .all(|dependency| dependency.name != "ents-verify"),
            "{} must not depend on ents-verify: verify/ is a sink layer nothing else may depend on",
            package.name
        );
    }
}
