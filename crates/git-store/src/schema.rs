//! The `.schema` version marker every document tree carries at its root.
//!
//! Per `docs/abstractions.adoc`'s "Typed tree" section, a `Facet` shape *is*
//! the storage format, so an incompatible change is normally invisible until
//! it silently fails to decode. The `.schema` marker turns that into a clean,
//! typed error: a tree written by a newer binary names a version this one
//! doesn't support, instead of tripping over fields it doesn't recognize.
//!
//! Only a tree-rooted document (struct, map, list, option, enum) has a
//! sibling slot to hold the marker; a scalar-rooted document (a bare
//! `String`, say) is left untouched. Every real meta-ref document is one of
//! the tree-shaped kinds — a bare scalar only ever shows up in `git-store`'s
//! own plumbing tests — so this is not a gap in practice.

use facet::Facet;
use gix::ObjectId;
use gix::objs::tree::{Entry as TreeEntry, EntryKind, EntryMode};
use gix::objs::{Find as _, FindExt as _, Kind, ObjectRef, Write as _};

use crate::Error;

/// The tree-root entry name a document's `.schema` marker is stored under.
/// The leading `.` keeps it out of the way of a struct's field names (never
/// valid Rust identifiers) and of a scalar-keyed map's own keys, which
/// [`crate::ref_segment_ok`] bars from starting with `.` when written
/// through [`crate::Store::store_map`].
const ENTRY_NAME: &str = ".schema";

/// A stored document's on-disk schema version, defaulting to `1` for every
/// `Facet` type. A document overrides [`SchemaVersion::VERSION`] only once
/// its shape changes incompatibly and old readers must be told they can't
/// parse the new tree; until that happens there is nothing to add. Migrating
/// is then just a normal write — a new tree committed on the ref's old tip —
/// not a bespoke migration engine.
pub trait SchemaVersion {
    /// This type's current on-disk schema version.
    const VERSION: u32 = 1;
}

impl<T: for<'a> Facet<'a>> SchemaVersion for T {}

/// Add a `.schema` marker for `version` as a sibling of `tree`'s existing
/// entries, replacing one already there. Returns `tree` unchanged when it
/// names a blob rather than a tree (see the module docs).
pub(crate) fn inject(
    odb: &gix::odb::Handle,
    tree: ObjectId,
    version: u32,
) -> Result<ObjectId, Error> {
    let Some(mut entries) = entries_of(odb, &tree)? else {
        return Ok(tree);
    };
    entries.retain(|entry| entry.filename != ENTRY_NAME);
    let marker = odb
        .write_buf(Kind::Blob, version.to_string().as_bytes())
        .map_err(|error| Error::Object(error.to_string()))?;
    entries.push(TreeEntry {
        mode: EntryMode::from(EntryKind::Blob),
        filename: ENTRY_NAME.into(),
        oid: marker,
    });
    entries.sort();
    odb.write(&gix::objs::Tree { entries })
        .map_err(|error| Error::Object(error.to_string()))
}

/// Remove `tree`'s `.schema` marker, if any, returning the stripped tree and
/// the version it named — `1` when the marker is absent, the pre-marker
/// on-disk format that must keep reading fine. Returns `tree` unchanged (and
/// version `1`) when it names a blob rather than a tree.
pub(crate) fn strip(odb: &gix::odb::Handle, tree: ObjectId) -> Result<(ObjectId, u32), Error> {
    let Some(mut entries) = entries_of(odb, &tree)? else {
        return Ok((tree, 1));
    };
    let Some(index) = entries
        .iter()
        .position(|entry| entry.filename == ENTRY_NAME)
    else {
        return Ok((tree, 1));
    };
    let marker = entries.remove(index);
    let version = read_version(odb, &marker.oid)?;
    entries.sort();
    let stripped = odb
        .write(&gix::objs::Tree { entries })
        .map_err(|error| Error::Object(error.to_string()))?;
    Ok((stripped, version))
}

/// Fail with a typed, named-versions error when `found` is newer than this
/// binary's `T::VERSION` — a future schema must never be mistaken for a
/// decode failure.
pub(crate) fn check<T: SchemaVersion>(found: u32) -> Result<(), Error> {
    if found > T::VERSION {
        return Err(Error::UnsupportedSchema {
            found,
            supported: T::VERSION,
        });
    }
    Ok(())
}

/// `tree`'s entries, or `None` when `tree` names a blob rather than a tree.
fn entries_of(odb: &gix::odb::Handle, tree: &ObjectId) -> Result<Option<Vec<TreeEntry>>, Error> {
    let mut buffer = Vec::new();
    let data = odb
        .try_find(tree, &mut buffer)
        .map_err(|error| Error::Object(error.to_string()))?
        .ok_or_else(|| Error::Object(format!("{tree} not found")))?;
    if data.kind != Kind::Tree {
        return Ok(None);
    }
    let object = data
        .decode()
        .map_err(|error| Error::Object(error.to_string()))?;
    let ObjectRef::Tree(tree_ref) = object else {
        return Ok(None);
    };
    Ok(Some(
        tree_ref
            .entries
            .into_iter()
            .map(|entry| TreeEntry {
                mode: entry.mode,
                filename: entry.filename.to_owned(),
                oid: entry.oid.to_owned(),
            })
            .collect(),
    ))
}

/// The integer named by the blob at `oid`, a `.schema` marker's value.
fn read_version(odb: &gix::odb::Handle, oid: &ObjectId) -> Result<u32, Error> {
    let mut buffer = Vec::new();
    let blob = odb
        .find_blob(oid, &mut buffer)
        .map_err(|error| Error::Object(error.to_string()))?;
    let text = std::str::from_utf8(blob.data)
        .map_err(|_utf8| Error::Object("`.schema` marker is not valid UTF-8".into()))?;
    text.parse()
        .map_err(|_parse| Error::Object(format!("`.schema` marker {text:?} is not an integer")))
}
