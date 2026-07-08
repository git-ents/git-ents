//! WS9: GC, compaction, and maintenance (`docs/scale-out.adoc`, "WS9 —
//! GC, compaction, maintenance").
//!
//! > Per-repo background effects serialized by advisory lock. Mark from
//! > RefStore via reachability artifacts; sweep via pack registry; cruft
//! > semantics where grace-based. Cache-ref TTL deletion; the
//! > consolidation effect from rule 4 lives here and is load-bearing.
//! > Reachability-artifact regeneration scheduled here.
//!
//! The pieces, one module each:
//!
//! - [`gc`] — mark ([`gix_reachability::gc_mark`], artifacts as
//!   accelerator) and sweep (over the pack registry, never a bucket
//!   listing and *structurally* never quarantine — see the module doc).
//! - [`cache`] — TTL eviction of cache refs and the consolidation effect,
//!   the only multi-ref cache writer (`docs/scale-out.adoc`, rule 4).
//! - [`lock`] — the per-repo advisory lock every maintenance run holds for
//!   its whole duration, so concurrent dispatchers can't double-run a
//!   repo: a Postgres advisory lock in cloud deployments, a file lock
//!   locally.
//! - [`schedule`] — the maintenance [`git_backend::EffectDef`]s and the
//!   [`schedule::Scheduler`] a server calls post-ingest to enqueue them on
//!   ref-update volume thresholds (including reachability regeneration via
//!   [`gix_reachability::maintenance::should_regenerate`], the trigger WS6
//!   left for this crate to schedule).
//! - [`collector`] — real [`backend_conformance::Collector`]s over the
//!   files and Tigris backends, closing the seam WS2 left open: the
//!   causal-collection-safety property now runs against a collection pass
//!   that actually collects, including the staging-timeout boundary
//!   (`docs/scale-out.adoc`, correctness rule 1).

pub mod cache;
pub mod collector;
pub mod gc;
pub mod lock;
pub mod schedule;

pub use git_backend::{Error, Result};
