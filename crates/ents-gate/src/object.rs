//! Minimal commit reading over `gix_object::Find` — the only object
//! access the gate performs (`arch.no-object-store-trait`: gitoxide's
//! traits are the object seam; no private store trait).

use gix_hash::ObjectId;
use gix_object::{CommitRef, Find, Kind};

use crate::error::{Error, Result};

/// The decoded pieces of one commit the gate judges or walks.
#[derive(Debug, Clone)]
pub(crate) struct CommitData {
    /// The raw commit bytes as stored — what a signature covers (minus
    /// the `gpgsig` header itself).
    pub raw: Vec<u8>,
    /// The tree the commit records.
    pub tree: ObjectId,
    /// Parents, in order.
    pub parents: Vec<ObjectId>,
    /// The full commit message, for trailer parsing.
    pub message: Vec<u8>,
}

/// Read `oid` and decode it as a commit; `Ok(None)` when the object
/// exists but is not a commit (the caller turns that into a refusal, not
/// an error).
pub(crate) fn read_commit(objects: &dyn Find, oid: ObjectId) -> Result<Option<CommitData>> {
    let mut buf = Vec::new();
    let data = objects
        .try_find(&oid, &mut buf)
        .map_err(|source| Error::Object { oid, source })?
        .ok_or(Error::Missing { oid })?;
    if data.kind != Kind::Commit {
        return Ok(None);
    }
    let raw = data.data.to_vec();
    let (tree, parents, message) = decode_commit(&raw, oid)?;
    Ok(Some(CommitData {
        tree,
        parents,
        message,
        raw,
    }))
}

fn decode_commit(raw: &[u8], oid: ObjectId) -> Result<(ObjectId, Vec<ObjectId>, Vec<u8>)> {
    let commit = CommitRef::from_bytes(raw, oid.kind()).map_err(|e| Error::Decode {
        oid,
        detail: e.to_string(),
    })?;
    Ok((
        commit.tree(),
        commit.parents().collect(),
        commit.message.to_vec(),
    ))
}

/// Like [`read_commit`], but a non-commit is an [`Error::Decode`] —
/// for walks where every node must be a commit.
pub(crate) fn expect_commit(objects: &dyn Find, oid: ObjectId) -> Result<CommitData> {
    read_commit(objects, oid)?.ok_or(Error::Decode {
        oid,
        detail: "expected a commit".into(),
    })
}

/// Whether `ancestor` is reachable from `descendant` by parent edges
/// (inclusive: a commit descends from itself) — the DAG sense of
/// `gate.fast-forward`.
pub(crate) fn descends_from(
    objects: &dyn Find,
    descendant: ObjectId,
    ancestor: ObjectId,
) -> Result<bool> {
    let mut queue = vec![descendant];
    let mut seen = std::collections::HashSet::new();
    while let Some(oid) = queue.pop() {
        if oid == ancestor {
            return Ok(true);
        }
        if !seen.insert(oid) {
            continue;
        }
        // A missing or non-commit ancestor object simply ends this path:
        // at pre-flight, history below the last fetch may be shallow.
        if let Some(commit) = read_commit(objects, oid).ok().flatten() {
            queue.extend(commit.parents);
        }
    }
    Ok(false)
}
