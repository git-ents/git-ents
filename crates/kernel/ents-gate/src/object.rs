//! Minimal commit reading over `gix_object::Find` — the only object
//! access the gate performs (`arch.no-object-store-trait`: gitoxide's
//! traits are the object seam; no private store trait).

use gix_hash::ObjectId;
use gix_object::{CommitRef, Find, Kind, TreeRef};

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
    let (tree, parents) = decode_commit(&raw, oid)?;
    Ok(Some(CommitData { tree, parents, raw }))
}

fn decode_commit(raw: &[u8], oid: ObjectId) -> Result<(ObjectId, Vec<ObjectId>)> {
    let commit = CommitRef::from_bytes(raw, oid.kind()).map_err(|e| Error::Decode {
        oid,
        detail: e.to_string(),
    })?;
    Ok((commit.tree(), commit.parents().collect()))
}

/// Like [`read_commit`], but a non-commit is an [`Error::Decode`] —
/// for walks where every node must be a commit.
pub(crate) fn expect_commit(objects: &dyn Find, oid: ObjectId) -> Result<CommitData> {
    read_commit(objects, oid)?.ok_or(Error::Decode {
        oid,
        detail: "expected a commit".into(),
    })
}

/// The raw bytes of the top-level tree entry named `entry`, or `None`
/// when `tree` has no such entry (`gate.identity-binding`: the gate reads
/// a binding field by tree-entry name, generically — it never decodes a
/// non-kernel entity type to recompute a refname, so it can bind a
/// review's `target` or a toolchain's `name` without depending on the
/// crate that owns that struct).
///
/// A named struct field serializes to a tree entry keyed by the field
/// name (`facet-git-tree`'s struct-to-tree mapping); a scalar field's
/// blob is its textual form, and a raw `[u8; 20]` oid field's blob is the
/// 20 raw bytes. This reads exactly that blob.
pub(crate) fn read_tree_entry(
    objects: &dyn Find,
    tree: ObjectId,
    entry: &str,
) -> Result<Option<Vec<u8>>> {
    let mut buf = Vec::new();
    let data = objects
        .try_find(&tree, &mut buf)
        .map_err(|source| Error::Object { oid: tree, source })?
        .ok_or(Error::Missing { oid: tree })?;
    if data.kind != Kind::Tree {
        return Ok(None);
    }
    let tree = TreeRef::from_bytes(data.data, tree.kind()).map_err(|e| Error::Decode {
        oid: tree,
        detail: e.to_string(),
    })?;
    let child = tree
        .entries
        .iter()
        .find(|e| e.filename == entry.as_bytes())
        .map(|e| e.oid.to_owned());
    let Some(child) = child else {
        return Ok(None);
    };
    let mut blob_buf = Vec::new();
    let blob = objects
        .try_find(&child, &mut blob_buf)
        .map_err(|source| Error::Object { oid: child, source })?
        .ok_or(Error::Missing { oid: child })?;
    Ok(Some(blob.data.to_vec()))
}

/// The names of every top-level entry in `tree`, for the strict-decode
/// disjointness check (`gate.identity-binding`: a genesis tree with an
/// entry that is not one of its entity type's fields refuses).
pub(crate) fn tree_entry_names(objects: &dyn Find, tree: ObjectId) -> Result<Vec<String>> {
    let mut buf = Vec::new();
    let data = objects
        .try_find(&tree, &mut buf)
        .map_err(|source| Error::Object { oid: tree, source })?
        .ok_or(Error::Missing { oid: tree })?;
    if data.kind != Kind::Tree {
        return Ok(Vec::new());
    }
    let parsed = TreeRef::from_bytes(data.data, tree.kind()).map_err(|e| Error::Decode {
        oid: tree,
        detail: e.to_string(),
    })?;
    Ok(parsed
        .entries
        .iter()
        .map(|e| String::from_utf8_lossy(e.filename).into_owned())
        .collect())
}

/// Every parentless commit reachable from `tip` by parent edges — the
/// genesis roots of a hash-identified entity's history
/// (`meta-ref.identity-binding`'s all-roots rule, `gate.identity-binding`).
///
/// Replaying a signed mutation commit as a fresh genesis is refused
/// because the walk reaches the original genesis (that mutation's own
/// parentless ancestor), not the replayed commit — a creation-time-only
/// check could not tell them apart. The walk descends through *every*
/// parent, so it holds across the merge commits divergence resolution and
/// adoption create (`gate.same-actor-divergence`, `gate.adoption-merge`).
/// A missing or non-commit object simply ends its path, exactly as
/// [`descends_from`] treats one: at pre-flight, history below the last
/// fetch may be shallow, and this is an advisory prediction there.
pub(crate) fn all_roots(objects: &dyn Find, tip: ObjectId) -> Result<Vec<ObjectId>> {
    let mut queue = vec![tip];
    let mut seen = std::collections::HashSet::new();
    let mut roots = Vec::new();
    while let Some(oid) = queue.pop() {
        if !seen.insert(oid) {
            continue;
        }
        match read_commit(objects, oid)? {
            Some(commit) if commit.parents.is_empty() => roots.push(oid),
            Some(commit) => queue.extend(commit.parents),
            None => {}
        }
    }
    Ok(roots)
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
