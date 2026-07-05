//! The Effect abstraction: anything a server runs against a push (CI, CD,
//! linting, versioning gates), decomposed into two pieces.
//!
//! [`definition`] holds an effect's static shape — its command, dependencies,
//! and toolchains, one ref per effect at `refs/meta/effects/<name>`.
//! [`results`] holds what running an effect against a commit produced, one
//! ref per effect per commit at `refs/meta/results/<effect>/<short-oid>`.
//! [`engine`] runs the effects a `post-receive` hook queues, in a Sprite
//! sandbox, and records their outcomes through `results`.
//!
//! This crate used to be `checks` (definitions in `git-ents-core`, execution
//! in `git-ents-server`) — see each module's migration note for the storage
//! rename that came with the split into its own crate.

pub mod definition;
pub mod engine;
pub mod results;
#[cfg(test)]
mod testutil;

pub use definition::{EFFECTS_NS, Effect, effect_ref, load, load_all, order, store};
pub use results::{CommitRuns, RESULTS_NS, Run, RunOutcome, Status, record, runs, update_run};
