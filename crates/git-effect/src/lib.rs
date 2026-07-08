//! The Effect abstraction: anything a server runs against a push (CI, CD,
//! linting, versioning gates), decomposed into two pieces.
//!
//! [`definition`] holds an effect's static shape — its command, dependencies,
//! toolchains, and cache, one ref per effect at `refs/meta/effects/<name>`.
//! [`results`] holds what running an effect against a commit produced, one
//! ref per effect per commit at `refs/meta/results/<effect>/<short-oid>`.
//! [`engine`] runs the effects a `post-receive` hook queues, in a Sprite
//! sandbox, and records their outcomes through `results`. [`cache`] persists
//! a read-write cache directory an effect's command can build up across runs.
//! [`executor`] adapts the Docker backend to [`git_backend::EffectExecutor`]
//! — `exec-local`, the executor seam's local half (`docs/scale-out.adoc`,
//! WS7).
//!
//! This crate used to be `checks` (definitions in `git-ents-core`, execution
//! in `git-ents-server`) — see each module's migration note for the storage
//! rename that came with the split into its own crate.

pub mod cache;
pub mod definition;
pub mod docker;
pub mod engine;
pub mod executor;
pub mod local;
pub mod results;
mod stream;
#[cfg(test)]
mod testutil;

pub use cache::{CACHE_NS, cache_dir, cache_ref};
pub use definition::{EFFECTS_NS, Effect, effect_ref, load, load_all, order, store};
pub use executor::LocalExecutor;
pub use results::{CommitRuns, RESULTS_NS, Run, RunOutcome, Status, record, runs, update_run};
