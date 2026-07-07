//! [`EffectExecutor`]: the seam between the effect engine and wherever an
//! effect actually runs.

use crate::Result;

/// The static definition of an effect to spawn: its name and the command
/// run for it (`None` for a composite effect that only aggregates
/// dependencies elsewhere), plus the sandbox image it runs in when it names
/// one. Mirrors the shape `git_effect::Effect` loads from
/// `refs/meta/effects/<name>` — kept as an independent, minimal type here
/// (rather than a dependency on `git-effect`) so this foundational crate
/// stays below the effect engine in the dependency graph, not above it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectDef {
    /// The name it is stored under (`refs/meta/effects/<name>`).
    pub name: String,
    /// The shell command run for the effect, or `None` for a composite
    /// effect that only aggregates its dependencies.
    pub command: Option<String>,
    /// The sandbox image the command runs in; `None` uses the default.
    pub image: Option<String>,
}

/// The materialized, ready-to-run inputs [`EffectExecutor::spawn`] hands to
/// a backend: the tree an effect runs against, each activated toolchain's
/// resolved `PATH` entry (keyed by toolchain name), and its cache
/// directory if it has one. Assembling these is "materialization"
/// (`docs/scale-out.adoc` correctness rule 6): manifest lookup,
/// `ObjectStore` read, hash verification, then handed here — the same one
/// code path regardless of which tier answered the read.
#[derive(Debug, Clone)]
pub struct MaterializedInputs {
    /// The tree the effect runs against.
    pub tree: gix_hash::ObjectId,
    /// Each activated toolchain's resolved `PATH` entry, keyed by name.
    pub toolchain_paths: std::collections::BTreeMap<String, String>,
    /// The effect's cache directory, if it names one.
    pub cache_dir: Option<String>,
}

/// A handle to a spawned effect. Opaque for now — WS7 (`exec-local`,
/// `exec-sprites`) adds the poll/await surface once a real executor backend
/// exists; this trait only needs to name the seam.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectHandle {
    /// A backend-chosen opaque identifier for the spawned effect.
    pub id: String,
}

/// Where a [`MaterializedInputs::tree`] actually runs: a sandboxed
/// subprocess today (`exec-local`), a Fly Machine later (`exec-sprites`).
/// Application code (the effect engine) is written once against this
/// trait; which backend answers `spawn` is a deployment detail.
pub trait EffectExecutor: Send + Sync {
    /// Spawn `effect` against `inputs`, returning a handle to the running
    /// effect. Does not block for completion.
    fn spawn(&self, effect: &EffectDef, inputs: MaterializedInputs) -> Result<EffectHandle>;
}
