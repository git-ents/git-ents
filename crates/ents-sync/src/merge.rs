//! The schema-aware three-way merge over typed trees — the one hard part
//! of sync (`sync.divergence-merge`).
//!
//! A meta-ref's tip is a git tree that `facet-git-tree` mapped directly
//! from a `#[derive(Facet)]` struct (`meta-ref.typed-tree`): a struct's
//! fields are tree entries, a nested struct or collection is a sub-tree,
//! and a scalar or string is a blob. Because the tree *is* the schema, a
//! structural three-way merge over the tree is a field-by-field merge over
//! the entity — never a textual merge over serialized bytes, which
//! `sync.divergence-merge` forbids.
//!
//! The merge is content-addressed, so it needs no diff heuristics: two
//! sides agree exactly when their object ids are equal. For each entry
//! (each field, each collection element) the classic three-way rule
//! applies against the merge-base tree `base`:
//!
//! - both sides equal → keep it (includes both-deleted and both-made-the-
//!   same-change);
//! - one side equals `base` → the *other* side changed it, so take the
//!   other (a deletion included);
//! - both sides changed it differently → recurse if both are sub-trees
//!   (a nested struct or collection merges field-by-field), otherwise it
//!   is a genuine conflict at that path.
//!
//! The result is either a clean merged tree (a new [`ObjectId`], ready to
//! become a merge tip whose signature makes it satisfy the tip invariant)
//! or the set of conflicting paths for a human to resolve.

use std::collections::{BTreeSet, HashMap};

use gix::bstr::{BString, ByteVec as _};
use gix_hash::ObjectId;
use gix_object::tree::{Entry as TreeEntry, EntryKind, EntryMode};
use gix_object::{Find, TreeRef, Write};

use crate::error::{Error, Result};

/// The outcome of a schema-aware three-way merge ([`three_way`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Merge {
    /// The two sides merged cleanly into this tree. A merge tip recording
    /// it, signed by an authorized member, satisfies the tip invariant
    /// (`sync.divergence-merge`).
    Clean(ObjectId),
    /// The two sides changed the same leaf differently. Each entry is a
    /// slash-joined path into the typed tree — a field name, or a field
    /// name and a collection index, exactly as `facet-git-tree` names
    /// them — so a caller can report *which* piece of the entity clashed.
    Conflict(Vec<BString>),
}

impl Merge {
    /// The clean merged tree, or `None` if the merge conflicted.
    #[must_use]
    pub fn tree(&self) -> Option<ObjectId> {
        match self {
            Merge::Clean(oid) => Some(*oid),
            Merge::Conflict(_) => None,
        }
    }
}

/// One entry of a tree, reduced to what the merge compares: its object id
/// and whether it is a sub-tree (so recursion is possible) or a leaf.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Slot {
    oid: ObjectId,
    mode: EntryMode,
}

impl Slot {
    fn is_tree(self) -> bool {
        self.mode.is_tree()
    }
}

/// Schema-aware three-way merge of two typed trees against their merge
/// base (`sync.divergence-merge`).
///
/// `base` is the tree of the merge-base commit — `None` when the two heads
/// share no common ancestor, which the merge treats as an empty base (every
/// entry looks added on both sides, so any difference is a conflict rather
/// than a silent pick). `ours` and `theirs` are the two divergent trees;
/// the merge is commutative, so their roles are symmetric.
///
/// Merged sub-trees and blobs are written into `objects`; the returned
/// [`Merge::Clean`] id is the root of the new tree.
///
/// # Errors
///
/// [`Error::Decode`] if a purported tree does not decode, [`Error::Object`]
/// or [`Error::Missing`] if one cannot be read, [`Error::Write`] if the
/// merged tree cannot be stored.
///
/// # Examples
///
/// A one-sided change is adopted wholesale — the field-level analogue of a
/// fast-forward:
///
/// ```
/// use ents_model::Issue;
/// use ents_sync::merge::{Merge, three_way};
/// use ents_testutil::ObjectStore;
///
/// let objects = ObjectStore::default();
/// let issue = Issue {
///     title: "t".into(), body: "b".into(), state: "open".into(),
///     assignees: vec![], labels: vec![],
/// };
/// let base = facet_git_tree::serialize_into(&issue, &objects).expect("ser");
/// let mut closed = issue.clone();
/// closed.state = "closed".into();
/// let theirs = facet_git_tree::serialize_into(&closed, &objects).expect("ser");
///
/// // ours == base (we changed nothing); theirs advanced.
/// let merged = three_way(&objects, Some(base), base, theirs).expect("merges");
/// assert_eq!(merged, Merge::Clean(theirs));
/// ```
// @relation(sync.divergence-merge, scope=function)
pub fn three_way(
    objects: &(impl Find + Write),
    base: Option<ObjectId>,
    ours: ObjectId,
    theirs: ObjectId,
) -> Result<Merge> {
    // Content addressing makes the fast path exact: equal ids are equal
    // subtrees, so identical sides need no walk at all.
    if ours == theirs {
        return Ok(Merge::Clean(ours));
    }

    let base_entries = match base {
        Some(oid) => read_tree(objects, oid)?,
        None => HashMap::new(),
    };
    let ours_entries = read_tree(objects, ours)?;
    let theirs_entries = read_tree(objects, theirs)?;

    let mut names: BTreeSet<&BString> = BTreeSet::new();
    names.extend(base_entries.keys());
    names.extend(ours_entries.keys());
    names.extend(theirs_entries.keys());

    let mut merged: Vec<TreeEntry> = Vec::new();
    let mut conflicts: Vec<BString> = Vec::new();

    for name in names {
        let o = ours_entries.get(name).copied();
        let t = theirs_entries.get(name).copied();
        let b = base_entries.get(name).copied();

        if o == t {
            // Both sides agree, including both-absent and both-identical-
            // change. Keep it when present.
            push_slot(&mut merged, name, o);
        } else if o == b {
            // Ours is unchanged from base, so theirs owns this entry —
            // a deletion included (`t == None`).
            push_slot(&mut merged, name, t);
        } else if t == b {
            // Symmetric: theirs is unchanged, ours owns this entry.
            push_slot(&mut merged, name, o);
        } else {
            // Both sides changed the same entry differently. A sub-tree on
            // both sides is a nested struct or collection that can itself
            // be merged field-by-field; anything else is a leaf conflict.
            match (o, t) {
                (Some(so), Some(st)) if so.is_tree() && st.is_tree() => {
                    let sub_base = b.filter(|s| s.is_tree()).map(|s| s.oid);
                    match three_way(objects, sub_base, so.oid, st.oid)? {
                        Merge::Clean(sub) => merged.push(TreeEntry {
                            mode: EntryKind::Tree.into(),
                            filename: name.clone(),
                            oid: sub,
                        }),
                        Merge::Conflict(paths) => {
                            for p in paths {
                                conflicts.push(join(name, &p));
                            }
                        }
                    }
                }
                _ => conflicts.push(name.clone()),
            }
        }
    }

    if conflicts.is_empty() {
        // git tree entries are canonically sorted; `TreeEntry`'s own `Ord`
        // is that order, matching how `facet-git-tree` writes trees.
        merged.sort();
        let oid = objects.write(&gix_object::Tree { entries: merged })?;
        Ok(Merge::Clean(oid))
    } else {
        conflicts.sort();
        Ok(Merge::Conflict(conflicts))
    }
}

/// Read the entries of the tree at `oid` into a name-keyed map.
fn read_tree(objects: &impl Find, oid: ObjectId) -> Result<HashMap<BString, Slot>> {
    let mut buf = Vec::new();
    let data = objects
        .try_find(&oid, &mut buf)
        .map_err(|source| Error::Object { oid, source })?
        .ok_or(Error::Missing { oid })?;
    let tree = TreeRef::from_bytes(data.data, oid.kind()).map_err(|e| Error::Decode {
        oid,
        detail: e.to_string(),
    })?;
    let mut map = HashMap::with_capacity(tree.entries.len());
    for entry in &tree.entries {
        map.insert(
            entry.filename.to_owned(),
            Slot {
                oid: entry.oid.to_owned(),
                mode: entry.mode,
            },
        );
    }
    Ok(map)
}

/// Append `slot` to `merged` under `name`, if it is present (a `None` slot
/// is an entry deleted on the winning side, so nothing is written).
fn push_slot(merged: &mut Vec<TreeEntry>, name: &BString, slot: Option<Slot>) {
    if let Some(slot) = slot {
        merged.push(TreeEntry {
            mode: slot.mode,
            filename: name.clone(),
            oid: slot.oid,
        });
    }
}

/// Join a parent entry name and a child path with `/`, the separator
/// `facet-git-tree` uses for nested tree paths.
fn join(parent: &BString, child: &BString) -> BString {
    let mut path = parent.clone();
    path.push_char('/');
    path.push_str(child);
    path
}
