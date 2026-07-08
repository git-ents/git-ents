//! [`Collector`]: the seam [`crate::causal_collection_safety`] tests
//! against. [`NoopCollector`] lets a backend with no GC wired up exercise
//! what a collection pass must never do — touch a staged/quarantined
//! object — without asserting behavior no collector implements. A backend
//! with real GC plugs its own `Collector` (reporting a real
//! [`Collector::staging_grace`] window, if it has one) into the same
//! property function instead — `git-maintenance` (WS9) does exactly that
//! for the files and Tigris backends, including the staging-timeout
//! boundary (`crates/git-maintenance/tests/conformance.rs`).

use std::time::Duration;

/// A hook onto a backend's collection (GC) pass, for
/// [`crate::causal_collection_safety`] to drive.
pub trait Collector {
    /// Run one collection pass now.
    fn collect(&self);

    /// The backend's staging grace window, if it bounds staging sessions
    /// with a time-based deadline (correctness rule 1 in
    /// `docs/scale-out.adoc`) rather than promotion alone. `None` for
    /// backends with no time-bounded staging, which is what the local file
    /// backends have today.
    fn staging_grace(&self) -> Option<Duration> {
        None
    }
}

/// A [`Collector`] that never collects anything and has no grace window —
/// today's stand-in for backends (`refstore-files`/`odb-files`) that have
/// no GC wired up yet. Running the suite against it still exercises the
/// real quarantine/promote path; it just never exercises the "a collection
/// pass actually reaped something" arm, which has no implementation to
/// test yet.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopCollector;

impl Collector for NoopCollector {
    fn collect(&self) {}
}
