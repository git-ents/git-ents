//! WS6: the reachability subsystem (`docs/scale-out.adoc`, "Reachability").
//!
//! > Negotiation, push connectivity checking, and GC mark are the same
//! > walk, and over a remote ODB that walk is the scaling wall: nothing may
//! > traverse Tigris object-by-object.
//!
//! This crate owns that shared walk ([`walk`], moved here from
//! `git-protocol`) plus the two accelerators that make it cheap at scale:
//!
//! - [`commitgraph::CommitGraph`] — commit OID -> (tree, parents,
//!   generation number), so a commit-parent traversal never has to round
//!   trip through [`git_backend::ObjectStore::read`] for a commit the graph
//!   already covers. This is the accelerator every walk benefits from
//!   regardless of which tips it starts from.
//! - [`reachable_set::ReachableSetArtifact`] — the full transitive object
//!   closure (commits, trees, blobs, tags) from one exact tip-frontier
//!   snapshot. Cheap to produce (one walk, at maintenance time) and, when a
//!   query's roots exactly match the frontier it was built from, cheap to
//!   consume: the cached closure *is* the answer, no walk at all. This is
//!   GC mark's steady-state case (roots = current ref tips, nothing moved
//!   since the last regeneration) and, whenever a client's `haves` happen to
//!   equal a server-known frontier, negotiation's.
//!
//! [`engine`] wires both into [`engine::accelerated_reachable`], the single
//! entry point [`native::negotiate`](../git_protocol/native/negotiate)-style
//! consumers call instead of the raw [`walk::reachable`]. [`store`] persists
//! artifacts beside packs, tracked in the pack registry
//! (`odb_tigris::registry::PackRegistry`). [`maintenance`] is the effect
//! that (re)generates them.
//!
//! # Correctness property
//!
//! > absence [or staleness] degrades speed, never answers
//!
//! [`commitgraph::CommitGraph::entry`] returns `None` for any commit it
//! doesn't cover — the walk falls back to an ordinary `ObjectStore` read for
//! exactly that commit, nothing more. [`engine::accelerated_reachable`]'s
//! whole-frontier fast path only ever fires on an *exact* set match between
//! the cached frontier and the query's roots; anything else — including a
//! frontier that is a strict ancestor of the current roots after new
//! commits landed — falls through to a full walk (itself still
//! commit-graph-accelerated wherever the graph covers it). Neither path can
//! ever produce a smaller-than-correct answer: the exact-match path returns
//! exactly what a from-scratch walk from those same roots would have
//! produced (that is what building the artifact ran), and the graph path
//! only ever substitutes a stored decode for an identical live one.
//!
//! # gc_mark
//!
//! [`gc_mark`] is WS9's entry point: the reachable set from every current
//! ref tip. GC itself (mark-and-sweep scheduling, cruft handling) is WS9's
//! job; this crate only proves out and tests the "mark" half.

mod codec;
pub mod commitgraph;
pub mod engine;
pub mod maintenance;
pub mod reachable_set;
pub mod store;
pub mod walk;

use std::collections::BTreeSet;

use git_backend::{RefName, RefStore};
use gix_hash::ObjectId;

pub use engine::{ArtifactBundle, accelerated_reachable};

/// A failure in this crate's artifact formats or the reachability walk
/// itself.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The underlying storage traits reported a failure.
    #[error(transparent)]
    Backend(#[from] git_backend::Error),
    /// A reachability walk found an object neither the graph nor `source`
    /// could resolve, and the walk was not marked lenient.
    #[error("missing object {0}")]
    MissingObject(ObjectId),
    /// Decoding a commit, tree, or tag object failed.
    #[error("could not decode object: {0}")]
    Decode(String),
    /// A serialized artifact was truncated, carried an unsupported version,
    /// or was otherwise malformed.
    #[error("malformed reachability artifact: {0}")]
    Format(String),
}

/// This crate's `Result` alias.
pub type Result<T> = std::result::Result<T, Error>;

/// Every ref's current tip in `refs` — the tip-frontier
/// [`maintenance::regenerate`] and [`gc_mark`] both walk from.
///
/// # Errors
///
/// Returns an error if the ref store cannot be read.
pub fn ref_tips(refs: &dyn RefStore) -> Result<Vec<ObjectId>> {
    refs.iter_prefix(&RefName::new("refs/"))?
        .map(|entry| entry.map(|(_name, oid)| oid).map_err(Error::from))
        .collect()
}

/// WS9's entry point: the set of every object reachable from `refs`'
/// current tips over `objects`, accelerated by whatever `artifacts` this
/// repo currently has (possibly none — see the module docs' correctness
/// property).
///
/// # Errors
///
/// Returns an error if the ref or object store cannot be read, or if the
/// walk finds a ref tip whose history is incomplete in `objects`.
pub fn gc_mark(
    refs: &dyn RefStore,
    objects: &dyn git_backend::ObjectStore,
    artifacts: &ArtifactBundle,
) -> Result<BTreeSet<ObjectId>> {
    let tips = ref_tips(refs)?;
    let source = walk::StoreSource::new(objects);
    engine::accelerated_reachable(tips, &source, |_id| false, false, artifacts)
}
