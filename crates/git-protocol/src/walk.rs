//! A generic reachability walk over whatever [`ObjectSource`] answers
//! `find`, shared by negotiation (`docs/scale-out.adoc`, "Reachability":
//! "Negotiation, push connectivity checking, and GC mark are the same
//! walk") and the ingest connectivity check.
//!
//! This is a naive, one-object-at-a-time walk through
//! [`git_backend::ObjectStore::read`] — correct, not fast. The doc calls out
//! pack generation over ranged reads as its own risk budget (Q6, WS5/WS6);
//! this walk is the thing that eventually needs a commit-graph/bitmap
//! accelerator (WS6) instead of visiting every object.

use std::collections::BTreeSet;

use git_backend::ObjectStore;
use gix_hash::ObjectId;
use gix_object::{CommitRef, Kind, TagRef, TreeRefIter};

use crate::{Error, Result};

/// Where [`reachable`] reads object kind/data from. Lets the same walk run
/// over a repository's promoted object store alone (negotiation, GC mark)
/// or a promoted store combined with a not-yet-promoted incoming pack
/// (ingest connectivity checking).
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
pub fn reachable(
    roots: impl IntoIterator<Item = ObjectId>,
    source: &dyn ObjectSource,
    mut stop: impl FnMut(&ObjectId) -> bool,
    lenient: bool,
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
