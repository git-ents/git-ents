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
    /// The effect's cache *name*, if it declares one. A name, not a path:
    /// where the cache lands is a property of each backend's own sandbox
    /// layout (a bind-mounted `/cache/<name>` in a local container, a
    /// persistent directory in a Sprite), so the backend maps the name
    /// itself — mirroring how the effect engine's backends each derive
    /// their own cache directory for the same declared cache.
    pub cache: Option<String>,
}

/// A handle to a spawned effect, returned by [`EffectExecutor::spawn`] and
/// consumed by [`EffectExecutor::wait`]. The id is backend-chosen and
/// opaque to callers: a worker-thread key for `exec-local`, a Fly Machine
/// id for `exec-sprites`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectHandle {
    /// A backend-chosen opaque identifier for the spawned effect.
    pub id: String,
}

/// The terminal state of a spawned effect, as observed by the
/// [`EffectExecutor`] that ran it (see [`EffectExecutor::wait`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectStatus {
    /// The command ran to completion and exited zero.
    Pass,
    /// The command ran to completion and exited non-zero.
    Fail,
    /// The executor could not run the command to an observable exit (a
    /// sandbox that would not start, a timeout, a lost worker).
    Error,
    /// The effect settled, but its outcome is not observable through this
    /// executor: it returns out-of-band, via the attested push of its
    /// results and cache refs with a worker member key
    /// (`docs/scale-out.adoc`, WS7 — the `exec-sprites` path, where the
    /// machine's termination tells the dispatcher only that the effect
    /// settled, and the recorded run refs carry the outcome).
    SettledRemotely,
}

/// Where a [`MaterializedInputs::tree`] actually runs: a sandboxed
/// subprocess today (`exec-local`), a Fly Machine hosted (`exec-sprites`).
/// Application code (the effect engine, the WS7 dispatcher) is written once
/// against this trait; which backend answers `spawn` is a deployment
/// detail.
pub trait EffectExecutor: Send + Sync {
    /// Spawn `effect` against `inputs`, returning a handle to the running
    /// effect. Does not block for completion.
    fn spawn(&self, effect: &EffectDef, inputs: MaterializedInputs) -> Result<EffectHandle>;

    /// Block until the effect behind `handle` settles, returning the
    /// terminal state this executor could observe. Consumes the handle's
    /// backend-side state: waiting twice on one handle is an error.
    fn wait(&self, handle: &EffectHandle) -> Result<EffectStatus>;
}
