//! Object-graph walks over gitoxide's `Find`/`Write` seams — the plumbing
//! shared by the merge machinery ([`crate::resolve`]) and forge transfer
//! ([`crate::transfer`]). No private object-access trait: gitoxide's own
//! traits are the seam (`arch.no-object-store-trait`).

use std::collections::HashSet;

use gix_hash::ObjectId;
use gix_object::{CommitRef, Find, Kind, TreeRef, Write};

use crate::error::{Error, Result};

/// The tree recorded by the commit at `oid`.
pub(crate) fn commit_tree(objects: &impl Find, oid: ObjectId) -> Result<ObjectId> {
    let mut buf = Vec::new();
    let data = objects
        .try_find(&oid, &mut buf)
        .map_err(|source| Error::Object { oid, source })?
        .ok_or(Error::Missing { oid })?;
    let commit = CommitRef::from_bytes(data.data, oid.kind()).map_err(|e| Error::Decode {
        oid,
        detail: e.to_string(),
    })?;
    Ok(commit.tree())
}

/// The parents of the commit at `oid`; an empty vec for a non-commit or a
/// root commit, so an incomplete (shallow) history simply ends a walk.
pub(crate) fn parents(objects: &impl Find, oid: ObjectId) -> Result<Vec<ObjectId>> {
    let mut buf = Vec::new();
    let Some(data) = objects
        .try_find(&oid, &mut buf)
        .map_err(|source| Error::Object { oid, source })?
    else {
        return Ok(Vec::new());
    };
    if data.kind != Kind::Commit {
        return Ok(Vec::new());
    }
    let commit = CommitRef::from_bytes(data.data, oid.kind()).map_err(|e| Error::Decode {
        oid,
        detail: e.to_string(),
    })?;
    Ok(commit.parents().collect())
}

/// Whether `descendant` reaches `ancestor` by parent edges (inclusive) —
/// the DAG sense of a fast-forward. Mirrors the gate's own descent check so
/// fetch advances a ref only when the remote truly descends from the local
/// tip (`gate.fast-forward`).
pub(crate) fn descends_from(
    objects: &impl Find,
    descendant: ObjectId,
    ancestor: ObjectId,
) -> Result<bool> {
    let mut stack = vec![descendant];
    let mut seen = HashSet::new();
    while let Some(oid) = stack.pop() {
        if oid == ancestor {
            return Ok(true);
        }
        if !seen.insert(oid) {
            continue;
        }
        stack.extend(parents(objects, oid)?);
    }
    Ok(false)
}

/// Copy every object reachable from `root` — the commit, its whole parent
/// chain, and every tree and blob those commits record — from `src` into
/// `dst`. Commit objects are copied verbatim, so their `gpgsig` signatures
/// travel with them: fetching a ref moves the complete audit history and
/// the signatures needed to verify it (`sync.forge-transfer`).
pub(crate) fn copy_closure(src: &impl Find, dst: &impl Write, root: ObjectId) -> Result<()> {
    let mut stack = vec![root];
    let mut seen = HashSet::new();
    while let Some(oid) = stack.pop() {
        if !seen.insert(oid) {
            continue;
        }
        let mut buf = Vec::new();
        let data = src
            .try_find(&oid, &mut buf)
            .map_err(|source| Error::Object { oid, source })?
            .ok_or(Error::Missing { oid })?;
        let kind = data.kind;
        let bytes = data.data.to_vec();
        dst.write_buf(kind, &bytes)?;
        match kind {
            Kind::Commit => {
                let commit =
                    CommitRef::from_bytes(&bytes, oid.kind()).map_err(|e| Error::Decode {
                        oid,
                        detail: e.to_string(),
                    })?;
                stack.push(commit.tree());
                stack.extend(commit.parents());
            }
            Kind::Tree => {
                let tree = TreeRef::from_bytes(&bytes, oid.kind()).map_err(|e| Error::Decode {
                    oid,
                    detail: e.to_string(),
                })?;
                stack.extend(tree.entries.iter().map(|e| e.oid.to_owned()));
            }
            Kind::Blob | Kind::Tag => {}
        }
    }
    Ok(())
}
