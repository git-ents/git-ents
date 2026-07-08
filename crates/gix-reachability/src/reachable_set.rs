//! [`ReachableSetArtifact`]: a full reachable-object-set snapshot for one
//! exact tip-frontier (`docs/scale-out.adoc`, "Reachability": "reachability
//! bitmaps" — this crate's equivalent, in its own hand-rolled format rather
//! than git's bitmap-index format, per this crate's module docs on why).
//!
//! One snapshot per `(repo_id, kind)` is kept (see
//! `odb_tigris::registry::PackRegistry`) — regenerating replaces it rather
//! than accumulating a history of frontiers. This is deliberately the
//! simplest artifact that helps: GC mark's roots are *always* "every current
//! ref tip", so between two maintenance runs with no intervening ref update
//! the frontier this snapshot was built from and GC mark's query roots are
//! identical, and [`crate::engine::accelerated_reachable`]'s exact-match
//! fast path returns the cached set with no walk at all. The same applies to
//! negotiation whenever a client's `haves` happen to equal a server-known
//! frontier (e.g. the tips as of its last fetch). Any other roots — a
//! frontier from before the most recent push, say — simply miss the fast
//! path and fall through to a full (still commit-graph-accelerated where
//! covered) walk: never a wrong answer, only a slower one.
//!
//! # Format (version 1)
//!
//! ```text
//! magic          "RGRS" (4 bytes)
//! version        1 (1 byte)
//! frontier_count u32 LE
//! frontier       frontier_count * 20 bytes, sorted ascending
//! object_count   u32 LE
//! objects        object_count * 20 bytes, sorted ascending
//! ```

use std::collections::BTreeSet;

use gix_hash::ObjectId;

use crate::Result;
use crate::codec::{Reader, Writer};
use crate::walk::{self, ObjectSource};

const MAGIC: &[u8; 4] = b"RGRS";
const VERSION: u8 = 1;

/// A snapshot of every object reachable from one exact tip-frontier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReachableSetArtifact {
    /// The exact set of tips this snapshot was computed from — the
    /// [`crate::engine::accelerated_reachable`] fast path only applies when
    /// a query's roots equal this set exactly.
    pub frontier: BTreeSet<ObjectId>,
    /// Every object (commits, trees, blobs, tags) reachable from
    /// `frontier`.
    pub objects: BTreeSet<ObjectId>,
}

impl ReachableSetArtifact {
    /// Compute the full reachable-object-set snapshot for `tips` over
    /// `source` — a plain, unaccelerated walk (this *is* how the
    /// accelerator gets built) with no `stop` boundary, since the whole
    /// point is a complete closure to cache.
    ///
    /// # Errors
    ///
    /// Returns an error if the walk finds a tip or ancestor `source` cannot
    /// resolve — a real inconsistency, not tolerated here the way a
    /// client's possibly-stale `have` is elsewhere.
    pub fn build(
        tips: impl IntoIterator<Item = ObjectId>,
        source: &dyn ObjectSource,
    ) -> Result<Self> {
        let frontier: BTreeSet<ObjectId> = tips.into_iter().collect();
        let objects = walk::reachable(frontier.iter().copied(), source, |_id| false, false)?;
        Ok(Self { frontier, objects })
    }

    /// Serialize to this module's binary format (version 1).
    #[must_use]
    pub fn serialize(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.header(MAGIC, VERSION);
        write_oid_set(&mut writer, &self.frontier);
        write_oid_set(&mut writer, &self.objects);
        writer.into_bytes()
    }

    /// Parse this module's binary format back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Format`] if the header, length, or any entry is
    /// malformed or truncated.
    pub fn deserialize(bytes: &[u8]) -> Result<Self> {
        let mut reader = Reader::new(bytes);
        reader.header(MAGIC, VERSION)?;
        let frontier = read_oid_set(&mut reader)?;
        let objects = read_oid_set(&mut reader)?;
        if !reader.at_end() {
            return Err(crate::Error::Format(
                "trailing bytes after reachable-set artifact".to_owned(),
            ));
        }
        Ok(Self { frontier, objects })
    }
}

fn write_oid_set(writer: &mut Writer, set: &BTreeSet<ObjectId>) {
    let count = u32::try_from(set.len()).unwrap_or(u32::MAX);
    writer.u32(count);
    for id in set {
        writer.oid(id);
    }
}

fn read_oid_set(reader: &mut Reader<'_>) -> Result<BTreeSet<ObjectId>> {
    let count = reader.u32()?;
    let mut set = BTreeSet::new();
    for _ in 0..count {
        set.insert(reader.oid()?);
    }
    Ok(set)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "unit test")]

    use std::collections::HashMap;

    use gix_object::Kind;

    use super::*;

    struct FakeBlobs(HashMap<ObjectId, (Kind, Vec<u8>)>);

    impl ObjectSource for FakeBlobs {
        fn find(&self, id: &ObjectId) -> Result<Option<(Kind, Vec<u8>)>> {
            Ok(self.0.get(id).cloned())
        }
    }

    fn oid(byte: u8) -> ObjectId {
        let mut bytes = [0_u8; 20];
        if let Some(last) = bytes.last_mut() {
            *last = byte;
        }
        ObjectId::from(bytes)
    }

    #[test]
    fn build_walks_a_single_blob_tip() {
        let blob = oid(1);
        let mut objects = HashMap::new();
        objects.insert(blob, (Kind::Blob, b"content".to_vec()));
        let source = FakeBlobs(objects);

        let artifact = ReachableSetArtifact::build([blob], &source).unwrap();
        assert_eq!(artifact.frontier, BTreeSet::from([blob]));
        assert_eq!(artifact.objects, BTreeSet::from([blob]));
    }

    #[test]
    fn round_trips_through_serialize_and_deserialize() {
        let a = oid(1);
        let b = oid(2);
        let mut objects = HashMap::new();
        objects.insert(a, (Kind::Blob, b"a".to_vec()));
        objects.insert(b, (Kind::Blob, b"b".to_vec()));
        let source = FakeBlobs(objects);

        let artifact = ReachableSetArtifact::build([a, b], &source).unwrap();
        let bytes = artifact.serialize();
        let read_back = ReachableSetArtifact::deserialize(&bytes).unwrap();
        assert_eq!(read_back, artifact);
    }

    #[test]
    fn deserialize_rejects_garbage() {
        let _error = ReachableSetArtifact::deserialize(b"nope").unwrap_err();
    }

    #[test]
    fn build_fails_on_a_missing_object() {
        let source = FakeBlobs(HashMap::new());
        let _error = ReachableSetArtifact::build([oid(1)], &source).unwrap_err();
    }
}
