//! A generic reachability walk over whatever [`ObjectSource`] answers
//! `find`, shared by negotiation, push connectivity checking, and GC mark
//! (`docs/scale-out.adoc`, "Reachability": "Negotiation, push connectivity
//! checking, and GC mark are the same walk").
//!
//! Moved here from `git-protocol` (WS6): this crate is where the
//! accelerator lives ([`crate::commitgraph`], [`crate::reachable_set`]), so
//! the walk they accelerate belongs beside them rather than in the protocol
//! crate that merely calls it. `git-protocol` still gets `ObjectSource`
//! and `reachable` through its own `walk` module, now a thin re-export.
//!
//! [`reachable`] is the plain, one-object-at-a-time walk — correct, not
//! fast. [`reachable_with_graph`] is the same walk with one addition: for
//! any id a [`crate::commitgraph::CommitGraph`] covers, its tree and
//! parents come from the graph instead of an [`ObjectSource::find`] round
//! trip. `reachable` is exactly `reachable_with_graph` with `graph: None`,
//! so there is one walk implementation, not two kept in sync by hand.

use std::collections::BTreeSet;

use git_backend::ObjectStore;
use gix_hash::ObjectId;
use gix_object::{CommitRef, Kind, TagRef, TreeRefIter};

use crate::commitgraph::CommitGraph;
use crate::{Error, Result};

/// Where a walk reads object kind/data from. Lets the same walk run over a
/// repository's promoted object store alone (negotiation, GC mark) or a
/// promoted store combined with a not-yet-promoted incoming pack (ingest
/// connectivity checking).
pub trait ObjectSource {
    /// The kind and raw content of `id`, or `None` if this source has never
    /// heard of it.
    fn find(&self, id: &ObjectId) -> Result<Option<(Kind, Vec<u8>)>>;
}

/// An [`ObjectSource`] over a repository's promoted [`ObjectStore`] alone.
pub struct StoreSource<'a> {
    store: &'a dyn ObjectStore,
}

impl<'a> StoreSource<'a> {
    /// Read only through `store`'s promoted view.
    pub fn new(store: &'a dyn ObjectStore) -> Self {
        Self { store }
    }
}

impl ObjectSource for StoreSource<'_> {
    fn find(&self, id: &ObjectId) -> Result<Option<(Kind, Vec<u8>)>> {
        if !self.store.contains(*id)? {
            return Ok(None);
        }
        let object = self.store.read(*id)?;
        Ok(Some((object.kind, object.data)))
    }
}

/// Walk every object reachable from `roots` via commit parents, commit/tag
/// targets, and tree entries (skipping gitlink/submodule entries, which name
/// a commit in a different repository's object space).
///
/// `stop` marks a boundary: when it returns `true` for an id, that id is
/// recorded as seen but never resolved or descended into — the caller
/// already knows it (and everything under it) is accounted for, e.g.
/// negotiation's haves closure, or the ingest connectivity check's existing
/// history.
///
/// When `lenient` is `false`, an id `stop` did not claim but `source` cannot
/// resolve is a connectivity failure ([`Error::MissingObject`]) — the ingest
/// check's use. When `true`, it is silently dropped instead — appropriate
/// for a client-supplied `have` the server never actually had, which is a
/// stale claim, not a corruption.
///
/// Plain, correct, not fast — see [`reachable_with_graph`] for the
/// commit-graph-accelerated form this degrades from.
pub fn reachable(
    roots: impl IntoIterator<Item = ObjectId>,
    source: &dyn ObjectSource,
    stop: impl FnMut(&ObjectId) -> bool,
    lenient: bool,
) -> Result<BTreeSet<ObjectId>> {
    reachable_with_graph(roots, source, stop, lenient, None)
}

/// [`reachable`], accelerated by `graph` when given: for any id `graph`
/// covers ([`CommitGraph::entry`] returns `Some`), its tree and parents come
/// from the graph — no [`ObjectSource::find`] call, and so (for a remote
/// object store) no read against it at all — instead of decoding the
/// commit. Anything the graph doesn't cover (trees, blobs, tags, and any
/// commit a stale or partial graph is missing) resolves through `source`
/// exactly as [`reachable`] would.
///
/// # Errors
///
/// Returns an error if decoding a resolved commit, tree, or tag fails, or
/// (when `lenient` is `false`) if an id neither `stop` nor `graph` nor
/// `source` accounts for.
pub fn reachable_with_graph(
    roots: impl IntoIterator<Item = ObjectId>,
    source: &dyn ObjectSource,
    mut stop: impl FnMut(&ObjectId) -> bool,
    lenient: bool,
    graph: Option<&CommitGraph>,
) -> Result<BTreeSet<ObjectId>> {
    let mut seen = BTreeSet::new();
    let mut stack: Vec<ObjectId> = roots.into_iter().collect();
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        if stop(&id) {
            continue;
        }

        if let Some(entry) = graph.and_then(|graph| graph.entry(&id)) {
            stack.push(entry.tree);
            stack.extend(entry.parents);
            continue;
        }

        let found = source.find(&id)?;
        let Some((kind, data)) = found else {
            if lenient {
                continue;
            }
            return Err(Error::MissingObject(id));
        };
        match kind {
            Kind::Commit => {
                let commit = CommitRef::from_bytes(&data, gix_hash::Kind::Sha1)
                    .map_err(|error| Error::Decode(error.to_string()))?;
                stack.push(commit.tree());
                stack.extend(commit.parents());
            }
            Kind::Tree => {
                for entry in TreeRefIter::from_bytes(&data, gix_hash::Kind::Sha1) {
                    let entry = entry.map_err(|error| Error::Decode(error.to_string()))?;
                    if entry.mode.kind() == gix_object::tree::EntryKind::Commit {
                        // A submodule gitlink: an object id in another
                        // repository's object space, never ours to resolve.
                        continue;
                    }
                    stack.push(entry.oid.to_owned());
                }
            }
            Kind::Tag => {
                let tag = TagRef::from_bytes(&data, gix_hash::Kind::Sha1)
                    .map_err(|error| Error::Decode(error.to_string()))?;
                stack.push(tag.target());
            }
            Kind::Blob => {}
        }
    }
    Ok(seen)
}
