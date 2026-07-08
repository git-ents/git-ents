//! Cache-namespace maintenance (`docs/scale-out.adoc`, rule 4 and WS9):
//! TTL eviction of cache refs, and the consolidation effect — the only
//! multi-ref cache writer, and load-bearing rather than hygiene: without
//! it the per-key writer discipline (one ref per key, no CAS contention
//! between concurrent workers) would grow the ref namespace without bound.
//!
//! # Eviction
//!
//! Rule 4: "Eviction = ref deletion + registry delete." [`evict_expired`]
//! is the ref-deletion half; the registry delete follows structurally from
//! the pack-lifetime rule (rule 5): cache objects live in their own packs,
//! so once their refs are gone the next [`crate::gc::collect`] finds those
//! packs fully unreachable and deletes them whole — registry delete plus
//! blob delete, never repack surgery.
//!
//! # Consolidation
//!
//! [`consolidate`] compacts `refs/cache/<namespace>/<key>` per-key refs
//! into one tree under [`git_backend::cache_ns::consolidated_ref`] and
//! deletes the per-key refs, all in **one atomic multi-ref transaction**
//! (the `RefStore` contract makes that contractual, not best-effort). The
//! new tree objects are staged and promoted *before* the transaction —
//! promoted-but-unreferenced objects are invisible to reachability until
//! the ref commit (rule 2), and this ordering means a failure at any point
//! leaves every key resolvable: before the transaction the per-key refs
//! still stand; after it the consolidated tree answers. There is no state
//! in between (see [`git_backend::cache_ns::resolve`], the shared read
//! path).

use std::collections::BTreeMap;
use std::time::Duration;

use git_backend::cache_ns;
use git_backend::{Expected, ObjectStore, PackStream, RefEdit, RefName, RefStore, TxOutcome};
use gix_hash::ObjectId;
use gix_object::WriteTo as _;

use crate::{Error, Result};

/// Delete every cache ref (both [`cache_ns::CACHE_PREFIXES`] namespaces)
/// whose latest reflog entry is older than `ttl` as of `now_secs` (seconds
/// since the epoch — injected rather than read from a clock so callers and
/// tests share one notion of "now"). Returns the refs evicted.
///
/// Each eviction is its own single-ref compare-and-swap transaction: a ref
/// a concurrent writer moves between read and delete is simply skipped
/// (the write refreshed it). A ref with no reflog entry is kept — with no
/// timestamp there is no expiry to assert, and keeping a cache entry is
/// always safe (rule 4: reconstructible, evictable *later*).
///
/// Plain `RefStore` transactions, not attested pushes: cache namespaces
/// are exempt from provenance (rule 4).
///
/// # Errors
///
/// Returns an error if the ref store fails; never because a CAS lost a
/// race.
pub fn evict_expired(refs: &dyn RefStore, ttl: Duration, now_secs: u64) -> Result<Vec<RefName>> {
    let mut evicted = Vec::new();
    for prefix in cache_ns::CACHE_PREFIXES {
        let entries: Vec<(RefName, ObjectId)> = refs
            .iter_prefix(&RefName::new(prefix))?
            .collect::<git_backend::Result<_>>()?;
        for (name, oid) in entries {
            let Some(written_secs) = latest_write_secs(refs, &name)? else {
                continue;
            };
            if now_secs.saturating_sub(written_secs) <= ttl.as_secs() {
                continue;
            }
            let outcome = refs.transaction(&[RefEdit {
                name: name.clone(),
                expected: Expected::MustExistAndMatch(oid),
                new: None,
            }])?;
            if matches!(outcome, TxOutcome::Applied) {
                evicted.push(name);
            }
        }
    }
    Ok(evicted)
}

/// The epoch seconds of `name`'s most recent reflog entry, or `None` if it
/// has no reflog.
fn latest_write_secs(refs: &dyn RefStore, name: &RefName) -> Result<Option<u64>> {
    match refs.log(name)?.next() {
        Some(entry) => Ok(Some(entry?.seconds)),
        None => Ok(None),
    }
}

/// A prepared consolidation: the tree already staged and promoted, and the
/// one multi-ref transaction that publishes it. Split from
/// [`consolidate`] so the atomicity boundary is testable: a failure after
/// [`prepare_consolidation`] but before [`commit_consolidation`] must
/// leave every key resolvable through its per-key ref.
#[derive(Debug)]
pub struct ConsolidationPlan {
    /// The consolidated tree's root, already promoted (visible to reads,
    /// unreachable until the transaction commits — rule 2).
    pub tree: ObjectId,
    /// The atomic edit batch: publish the consolidated ref, delete every
    /// per-key ref, all-or-nothing.
    pub edits: Vec<RefEdit>,
    /// How many keys this plan consolidates.
    pub keys: usize,
}

/// Build `namespace`'s consolidation: read every per-key ref, merge with
/// the existing consolidated tree (per-key wins — it is always current,
/// see [`cache_ns::resolve`]), write the merged tree's objects through the
/// staged-pack path (no `write_loose` exists — rule 2's staging applies to
/// maintenance too), promote them, and return the transaction that
/// publishes the result. `None` when there are no per-key refs to compact.
///
/// # Errors
///
/// Returns an error if any store operation fails, or if two keys collide
/// as blob-vs-directory in the tree (e.g. keys `a` and `a/b`) — a
/// namespace whose writer permits that cannot be consolidated into a tree.
pub fn prepare_consolidation(
    namespace: &str,
    refs: &dyn RefStore,
    objects: &dyn ObjectStore,
) -> Result<Option<ConsolidationPlan>> {
    let prefix = cache_ns::per_key_prefix(namespace);
    let per_key: Vec<(RefName, ObjectId)> = refs
        .iter_prefix(&prefix)?
        .collect::<git_backend::Result<_>>()?;
    if per_key.is_empty() {
        return Ok(None);
    }

    let consolidated = cache_ns::consolidated_ref(namespace);
    let existing = refs.get(&consolidated)?;

    let mut root = match existing {
        Some(tree) => read_tree_node(objects, tree)?,
        None => Node::Dir(BTreeMap::new()),
    };
    for (name, oid) in &per_key {
        let key = name
            .as_str()
            .strip_prefix(prefix.as_str())
            .ok_or_else(|| Error::RefStore(format!("{name} is not under {prefix}")))?;
        insert_key(&mut root, key, *oid)?;
    }

    let mut new_objects = Vec::new();
    let tree = write_node(&root, &mut new_objects)?;
    let pack = git_protocol::pack::build_pack(&new_objects)
        .map_err(|error| Error::ObjectStore(error.to_string()))?;
    let quarantine = objects.stage_pack(PackStream::new(std::io::Cursor::new(pack)))?;
    objects.promote(quarantine)?;

    let mut edits = vec![RefEdit {
        name: consolidated,
        expected: match existing {
            Some(old) => Expected::MustExistAndMatch(old),
            None => Expected::MustNotExist,
        },
        new: Some(tree),
    }];
    let keys = per_key.len();
    edits.extend(per_key.into_iter().map(|(name, oid)| RefEdit {
        name,
        expected: Expected::MustExistAndMatch(oid),
        new: None,
    }));

    Ok(Some(ConsolidationPlan { tree, edits, keys }))
}

/// Apply a [`ConsolidationPlan`]'s transaction. `Ok(true)` when it
/// applied; `Ok(false)` when a concurrent writer moved any touched ref and
/// the whole batch was rejected — nothing changed (all-or-nothing), the
/// next maintenance run re-prepares against the new state.
///
/// # Errors
///
/// Returns an error only if the ref store itself fails.
pub fn commit_consolidation(refs: &dyn RefStore, plan: &ConsolidationPlan) -> Result<bool> {
    Ok(matches!(refs.transaction(&plan.edits)?, TxOutcome::Applied))
}

/// The consolidation effect (`docs/scale-out.adoc`, rule 4): compact
/// `namespace`'s per-key cache refs into its consolidated tree ref in one
/// atomic multi-ref transaction. Returns how many keys were consolidated —
/// `0` when there was nothing to do or a concurrent writer won the race.
///
/// # Errors
///
/// See [`prepare_consolidation`] and [`commit_consolidation`].
pub fn consolidate(
    namespace: &str,
    refs: &dyn RefStore,
    objects: &dyn ObjectStore,
) -> Result<usize> {
    let Some(plan) = prepare_consolidation(namespace, refs, objects)? else {
        return Ok(0);
    };
    if commit_consolidation(refs, &plan)? {
        Ok(plan.keys)
    } else {
        Ok(0)
    }
}

/// An in-memory consolidated tree under construction: cache blobs at the
/// leaves, directories per key path segment.
enum Node {
    Leaf(ObjectId),
    Dir(BTreeMap<String, Node>),
}

/// Insert `key` (slash-separated path) pointing at `oid` into `root`,
/// failing on a blob-vs-directory collision rather than silently dropping
/// either side.
fn insert_key(root: &mut Node, key: &str, oid: ObjectId) -> Result<()> {
    let mut node = root;
    let mut segments = key.split('/').peekable();
    while let Some(segment) = segments.next() {
        let Node::Dir(children) = node else {
            return Err(Error::ObjectStore(format!(
                "cache key {key} collides with another key at segment {segment}"
            )));
        };
        if segments.peek().is_none() {
            children.insert(segment.to_owned(), Node::Leaf(oid));
            return Ok(());
        }
        node = children
            .entry(segment.to_owned())
            .or_insert_with(|| Node::Dir(BTreeMap::new()));
    }
    Ok(())
}

/// Read an existing consolidated tree back into a [`Node`]: tree entries
/// recurse, everything else is a leaf.
fn read_tree_node(objects: &dyn ObjectStore, tree: ObjectId) -> Result<Node> {
    let object = objects.read(tree)?;
    if object.kind != gix_object::Kind::Tree {
        return Err(Error::ObjectStore(format!(
            "consolidated ref points at a {:?}, not a tree",
            object.kind
        )));
    }
    let parsed = gix_object::TreeRef::from_bytes(&object.data, gix_hash::Kind::Sha1)
        .map_err(|error| Error::ObjectStore(format!("malformed consolidated tree: {error}")))?;
    let mut children = BTreeMap::new();
    for entry in parsed.entries {
        let name = std::str::from_utf8(entry.filename)
            .map_err(|_error| {
                Error::ObjectStore("consolidated tree entry name is not UTF-8".to_owned())
            })?
            .to_owned();
        let child = if entry.mode.is_tree() {
            read_tree_node(objects, entry.oid.to_owned())?
        } else {
            Node::Leaf(entry.oid.to_owned())
        };
        children.insert(name, child);
    }
    Ok(Node::Dir(children))
}

/// Write `node` (and every subtree) as tree objects, appending each new
/// tree to `out` for packing, returning `node`'s id. Leaves are recorded
/// as plain blobs — cache values are content blobs, their bytes already in
/// the store.
fn write_node(node: &Node, out: &mut Vec<git_protocol::pack::PackObject>) -> Result<ObjectId> {
    match node {
        Node::Leaf(oid) => Ok(*oid),
        Node::Dir(children) => {
            let mut entries = Vec::with_capacity(children.len());
            for (name, child) in children {
                let oid = write_node(child, out)?;
                entries.push(gix_object::tree::Entry {
                    mode: match child {
                        Node::Leaf(_oid) => gix_object::tree::EntryKind::Blob.into(),
                        Node::Dir(_children) => gix_object::tree::EntryKind::Tree.into(),
                    },
                    filename: name.as_str().into(),
                    oid,
                });
            }
            entries.sort();
            let tree = gix_object::Tree { entries };
            let mut data = Vec::new();
            tree.write_to(&mut data)?;
            let oid = gix_object::compute_hash(gix_hash::Kind::Sha1, gix_object::Kind::Tree, &data)
                .map_err(|error| Error::ObjectStore(error.to_string()))?;
            out.push(git_protocol::pack::PackObject {
                id: oid,
                kind: gix_object::Kind::Tree,
                data,
            });
            Ok(oid)
        }
    }
}
