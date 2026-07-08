//! The cache ref namespaces (`docs/scale-out.adoc`, correctness rule 4)
//! and the one lookup both their writer (`git-cache-proxy`) and their
//! maintainer (`git-maintenance`, WS9) must agree on.
//!
//! Rule 4's contract: `refs/cache/*` and `refs/meta/cache/*` are
//! evictable, reconstructible, and exempt from provenance. Concurrent
//! writers use per-key refs; a consolidation effect — the only multi-ref
//! cache writer — compacts them into one tree under a consolidated ref.
//! After a consolidation, a key's bytes are reachable through *either* its
//! per-key ref (not yet consolidated) or the consolidated tree (already
//! compacted); [`resolve`] is the read path that consults both, defined
//! here so the proxy's GET and the maintenance tests resolve keys through
//! the identical code.

use gix_hash::ObjectId;

use crate::{ObjectStore, RefName, RefStore, Result};

/// Every cache ref namespace (`docs/scale-out.adoc`, rule 4). Anything
/// under these prefixes is evictable and reconstructible; nothing outside
/// them is ever treated as cache by maintenance.
pub const CACHE_PREFIXES: [&str; 2] = ["refs/cache/", "refs/meta/cache/"];

/// Whether `name` lies in a cache namespace ([`CACHE_PREFIXES`]).
#[must_use]
pub fn is_cache_ref(name: &RefName) -> bool {
    CACHE_PREFIXES
        .iter()
        .any(|prefix| name.as_str().starts_with(prefix))
}

/// The prefix per-key cache refs for `namespace` live under —
/// `refs/cache/<namespace>/`, one ref per key below it.
#[must_use]
pub fn per_key_prefix(namespace: &str) -> RefName {
    RefName::new(format!("refs/cache/{namespace}/"))
}

/// The ref the consolidation effect compacts `namespace`'s per-key refs
/// into: points at a tree whose path `<key>` holds the key's blob.
/// Deliberately *not* under [`per_key_prefix`] (a sibling `consolidated/`
/// namespace instead), so it can never collide with a key's own ref — and
/// it stays inside `refs/cache/`, so rule 4 (evictable, own packs, exempt
/// from provenance) applies to it exactly as to the refs it replaces.
#[must_use]
pub fn consolidated_ref(namespace: &str) -> RefName {
    RefName::new(format!("refs/cache/consolidated/{namespace}"))
}

/// Resolve cache key `key` in `namespace` to its blob's id: the per-key
/// ref first — one ref lookup, a hit for every key written since the last
/// consolidation, keeping the common-case GET at a single lookup — then
/// the consolidated tree (one ref lookup plus a tree descent) for keys
/// already compacted. `None` when neither knows the key.
///
/// Per-key-first is also the *correct* order, not just the fast one: the
/// consolidation transaction deletes a per-key ref in the same atomic
/// multi-ref transaction that publishes the consolidated tree, so a
/// present per-key ref is always current, never a stale shadow of a
/// consolidated entry.
///
/// # Errors
///
/// Returns an error if the ref store or object store fails, or if a
/// consolidated tree object is malformed.
pub fn resolve(
    refs: &dyn RefStore,
    objects: &dyn ObjectStore,
    namespace: &str,
    key: &str,
) -> Result<Option<ObjectId>> {
    let per_key = RefName::new(format!("refs/cache/{namespace}/{key}"));
    if let Some(oid) = refs.get(&per_key)? {
        return Ok(Some(oid));
    }
    let Some(root) = refs.get(&consolidated_ref(namespace))? else {
        return Ok(None);
    };
    tree_path(objects, root, key)
}

/// Descend from tree `root` along `/`-separated `path`, returning the id
/// the final segment names, or `None` if any segment is absent.
///
/// # Errors
///
/// Returns an error if an object read fails or a tree is malformed.
pub fn tree_path(
    objects: &dyn ObjectStore,
    root: ObjectId,
    path: &str,
) -> Result<Option<ObjectId>> {
    let mut current = root;
    let mut segments = path.split('/').peekable();
    while let Some(segment) = segments.next() {
        let object = objects.read(current)?;
        if object.kind != gix_object::Kind::Tree {
            return Ok(None);
        }
        let tree = gix_object::TreeRef::from_bytes(&object.data, gix_hash::Kind::Sha1)
            .map_err(|error| crate::Error::ObjectStore(format!("malformed tree: {error}")))?;
        let Some(entry) = tree
            .entries
            .iter()
            .find(|entry| entry.filename == segment.as_bytes())
        else {
            return Ok(None);
        };
        let child = entry.oid.to_owned();
        if segments.peek().is_none() {
            return Ok(Some(child));
        }
        current = child;
    }
    Ok(None)
}
