//! Schema-aware three-way merge of two encodings of the same [`facet::Facet`]
//! type that both descend from a common tree.
//!
//! [`Store::store`](crate::Store::store) and
//! [`Store::store_authored`](crate::Store::store_authored) call
//! [`three_way_merge`] when a concurrent writer has moved a ref since they
//! read it: the document's `Facet` shape tells the walk which subtrees are
//! safe to recurse into (structs, scalar-keyed maps) and which are atomic
//! (scalars, `Option`, enums), so disjoint edits combine and a genuine
//! same-leaf clash fails with [`Error::Conflict`] instead of picking a winner.

use std::collections::BTreeSet;

use facet::{Def, Facet, Shape, StructKind, Type, UserType};
use gix::ObjectId;
use gix::objs::tree::{Entry as TreeEntry, EntryKind, EntryMode};
use gix::objs::{FindExt as _, Tree, Write as _};

use crate::Error;

/// A tree entry as read off a parent tree, keyed elsewhere by its name: the
/// object id and mode to keep as-is, or to fold into a freshly written tree.
#[derive(Clone, Copy)]
struct Child {
    oid: ObjectId,
    mode: EntryMode,
}

/// Merge `ours` and `theirs` — two trees encoding a `T`, both descended from
/// `base` — into a single tree.
pub(crate) fn three_way_merge<T: for<'a> Facet<'a>>(
    base: ObjectId,
    ours: ObjectId,
    theirs: ObjectId,
    odb: &gix::odb::Handle,
) -> Result<ObjectId, Error> {
    let mode = EntryMode::from(EntryKind::Tree);
    let merged = merge_node(
        T::SHAPE,
        Some(Child { oid: base, mode }),
        Child { oid: ours, mode },
        Child { oid: theirs, mode },
        odb,
    )?;
    Ok(merged.oid)
}

/// How a shape's tree is safe to recurse into during a merge.
enum Classify {
    /// A named or positional struct: per-field recursion.
    Struct {
        fields: &'static [facet::Field],
        positional: bool,
    },
    /// A scalar-keyed map: per-key recursion, keys named by their textual form.
    Map { value: &'static Shape },
    /// A scalar, `Option`, enum, or anything else: no recursion, only equality.
    Atomic,
}

fn classify(shape: &'static Shape) -> Classify {
    if let Type::User(UserType::Struct(st)) = shape.ty
        && !matches!(st.kind, StructKind::Unit)
    {
        let positional = matches!(st.kind, StructKind::Tuple | StructKind::TupleStruct);
        return Classify::Struct {
            fields: st.fields,
            positional,
        };
    }
    if let Def::Map(md) = shape.def
        && matches!(md.k.def, Def::Scalar)
    {
        return Classify::Map { value: md.v };
    }
    Classify::Atomic
}

fn merge_node(
    shape: &'static Shape,
    base: Option<Child>,
    ours: Child,
    theirs: Child,
    odb: &gix::odb::Handle,
) -> Result<Child, Error> {
    if ours.oid == theirs.oid {
        return Ok(ours);
    }
    if let Some(base) = base {
        if base.oid == ours.oid {
            return Ok(theirs);
        }
        if base.oid == theirs.oid {
            return Ok(ours);
        }
    }
    match classify(shape) {
        Classify::Struct { fields, positional } => {
            merge_struct(fields, positional, base, ours, theirs, odb)
        }
        Classify::Map { value } => merge_map(value, base, ours, theirs, odb),
        // Both sides changed a scalar, `Option`, or enum leaf: never
        // synthesize a partial value, fail closed instead.
        Classify::Atomic => Err(Error::Conflict),
    }
}

fn merge_struct(
    fields: &'static [facet::Field],
    positional: bool,
    base: Option<Child>,
    ours: Child,
    theirs: Child,
    odb: &gix::odb::Handle,
) -> Result<Child, Error> {
    let base_entries = base.map(|b| tree_entries(b.oid, odb)).transpose()?;
    let ours_entries = tree_entries(ours.oid, odb)?;
    let theirs_entries = tree_entries(theirs.oid, odb)?;

    let mut out = Vec::with_capacity(fields.len());
    for (i, field) in fields.iter().enumerate() {
        let name = if positional {
            format!("{i:04}")
        } else {
            field.name.to_owned()
        };
        let ours_child = find(&ours_entries, &name)
            .ok_or_else(|| Error::Object(format!("field {name:?} missing from ours tree")))?;
        let theirs_child = find(&theirs_entries, &name)
            .ok_or_else(|| Error::Object(format!("field {name:?} missing from theirs tree")))?;
        let base_child = base_entries
            .as_ref()
            .and_then(|entries| find(entries, &name));
        let merged = merge_node(field.shape.get(), base_child, ours_child, theirs_child, odb)?;
        out.push(TreeEntry {
            mode: merged.mode,
            filename: name.into(),
            oid: merged.oid,
        });
    }
    write_tree(odb, out)
}

fn merge_map(
    value_shape: &'static Shape,
    base: Option<Child>,
    ours: Child,
    theirs: Child,
    odb: &gix::odb::Handle,
) -> Result<Child, Error> {
    let base_entries = base
        .map(|b| tree_entries(b.oid, odb))
        .transpose()?
        .unwrap_or_default();
    let ours_entries = tree_entries(ours.oid, odb)?;
    let theirs_entries = tree_entries(theirs.oid, odb)?;

    let mut keys: BTreeSet<&str> = BTreeSet::new();
    keys.extend(ours_entries.iter().map(|(name, _)| name.as_str()));
    keys.extend(theirs_entries.iter().map(|(name, _)| name.as_str()));

    let mut out = Vec::new();
    for key in keys {
        let base_child = find(&base_entries, key);
        let ours_child = find(&ours_entries, key);
        let theirs_child = find(&theirs_entries, key);
        let resolved = match (ours_child, theirs_child) {
            (Some(o), Some(t)) => Some(merge_node(value_shape, base_child, o, t, odb)?),
            // Present only on our side: either we added it fresh (no base
            // entry), or theirs deleted an entry we left untouched (drop it),
            // or theirs deleted an entry we also changed (conflict).
            (Some(o), None) => match base_child {
                Some(b) if b.oid == o.oid => None,
                Some(_) => return Err(Error::Conflict),
                None => Some(o),
            },
            (None, Some(t)) => match base_child {
                Some(b) if b.oid == t.oid => None,
                Some(_) => return Err(Error::Conflict),
                None => Some(t),
            },
            (None, None) => None,
        };
        if let Some(child) = resolved {
            out.push(TreeEntry {
                mode: child.mode,
                filename: key.into(),
                oid: child.oid,
            });
        }
    }
    write_tree(odb, out)
}

fn find(entries: &[(String, Child)], name: &str) -> Option<Child> {
    entries
        .iter()
        .find(|(entry_name, _)| entry_name == name)
        .map(|(_, child)| *child)
}

fn tree_entries(oid: ObjectId, odb: &gix::odb::Handle) -> Result<Vec<(String, Child)>, Error> {
    let mut buf = Vec::new();
    let tree = odb
        .find_tree(&oid, &mut buf)
        .map_err(|error| Error::Object(error.to_string()))?;
    tree.entries
        .iter()
        .map(|entry| {
            let name = std::str::from_utf8(entry.filename)
                .map_err(|_error| Error::Object("tree entry name is not valid UTF-8".into()))?
                .to_owned();
            Ok((
                name,
                Child {
                    oid: entry.oid.to_owned(),
                    mode: entry.mode,
                },
            ))
        })
        .collect()
}

fn write_tree(odb: &gix::odb::Handle, mut entries: Vec<TreeEntry>) -> Result<Child, Error> {
    entries.sort();
    let oid = odb
        .write(&Tree { entries })
        .map_err(|error| Error::Object(error.to_string()))?;
    Ok(Child {
        oid,
        mode: EntryMode::from(EntryKind::Tree),
    })
}
