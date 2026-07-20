//! Round-trip fixture for [`Binding::deserialize`]/[`Binding::serialize_into`]
//! against the *existing* stored anchor format, captured from the current
//! code before this phase's changes: a byte-for-byte guarantee that
//! `Binding::Position` decodes, and re-encodes, the exact tree
//! `facet_git_tree::serialize_into(&anchor, ...)` has always produced.
//!
//! Every oid below is content-addressed from the bytes reconstructed in
//! this file, so a single corrupted byte in any hex constant — or in the
//! reconstructed content itself — fails an `assert_eq!` here rather than
//! silently drifting.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration test"
)]

use ents_anchor::{Binding, LineRange};
use facet_git_tree::ObjectStore;
use gix_object::tree::{Entry, EntryKind, EntryMode};
use gix_object::{Kind, Tree, Write as _};

/// The root tree's oid, exactly as the current code produces it for an
/// anchor at `file.txt`, lines 3..=4, in a 10-line numbered file.
const ROOT: &str = "002b45e6824a3a9723ebc245104426c43ccf91be";

/// `blob` entry: [`ents_anchor::Anchor::blob`]'s 20 raw bytes, embedded —
/// equal to `CONTENT_OID`'s own bytes, by content addressing
/// (`anchor.retention`).
const BLOB_ENTRY_OID: &str = "4a3354a7c472ad13ffd9fb0e30d9a8fd66efd0b5";
const BLOB_ENTRY_RAW: &str = "fa2da6e55caa540725b55c04d13f1e42b4c725ce";

/// `commit` entry: [`ents_anchor::Anchor::commit`]'s 20 raw bytes, embedded
/// (an arbitrary, best-effort commit id — it need not resolve to a real
/// object in this fixture).
const COMMIT_ENTRY_OID: &str = "a662e760fcd5534f59d7c7d72e401a646ac1a88f";
const COMMIT_ENTRY_RAW: &str = "92cf309c4efcf8698a5bd8f82d56f68fd38cc963";

/// `content` entry: the anchored blob's own bytes, `"line 1\n"` through
/// `"line 10\n"`.
const CONTENT_OID: &str = "fa2da6e55caa540725b55c04d13f1e42b4c725ce";
/// `context` entry: a three-line margin around lines 3..=4, `"line 1\n"`
/// through `"line 7\n"`.
const CONTEXT_OID: &str = "734156dc73cccb9703067e6366f3d09266e090dd";

const LINES_OID: &str = "b76e73cdb409fa346566f18e8f054dbdf04a7304";
const LINES_SOME_OID: &str = "3433ad944c71f4b15c4de9e87568ae4cf03feb50";
const LINES_END_OID: &str = "bf0d87ab1b2b0ec1a11a3973d2845b42413d9767";
const LINES_START_OID: &str = "e440e5c842586965a7fb77deda2eca68612b1f53";

const PATH_OID: &str = "4c330738cc959751fb6760a91a50d9e58cfe5cb9";

fn oid(hex: &str) -> gix::ObjectId {
    gix::ObjectId::from_hex(hex.as_bytes()).expect("valid hex oid")
}

fn numbered(range: std::ops::RangeInclusive<u32>) -> String {
    range.map(|n| format!("line {n}\n")).collect()
}

fn write_blob(store: &ObjectStore, bytes: &[u8]) -> gix::ObjectId {
    store.write_buf(Kind::Blob, bytes).expect("write blob")
}

fn write_tree(store: &ObjectStore, mut entries: Vec<Entry>) -> gix::ObjectId {
    entries.sort();
    store.write(&Tree { entries }).expect("write tree")
}

fn entry(name: &str, kind: EntryKind, id: gix::ObjectId) -> Entry {
    Entry {
        mode: EntryMode::from(kind),
        filename: name.into(),
        oid: id,
    }
}

/// Reconstruct the fixture's object set with `gix_object::Tree` +
/// `gix_object::Write`, asserting every intermediate oid along the way
/// (item 1 of the fixture contract) before returning the finished store.
fn build_fixture() -> (gix::ObjectId, ObjectStore) {
    let store = ObjectStore::default();

    let blob_entry = write_blob(&store, oid(BLOB_ENTRY_RAW).as_slice());
    assert_eq!(blob_entry.to_string(), BLOB_ENTRY_OID);

    let commit_entry = write_blob(&store, oid(COMMIT_ENTRY_RAW).as_slice());
    assert_eq!(commit_entry.to_string(), COMMIT_ENTRY_OID);

    let content = write_blob(&store, numbered(1..=10).as_bytes());
    assert_eq!(content.to_string(), CONTENT_OID);

    let context = write_blob(&store, numbered(1..=7).as_bytes());
    assert_eq!(context.to_string(), CONTEXT_OID);

    let end = write_blob(&store, b"4");
    assert_eq!(end.to_string(), LINES_END_OID);
    let start = write_blob(&store, b"3");
    assert_eq!(start.to_string(), LINES_START_OID);
    let some = write_tree(
        &store,
        vec![
            entry("end", EntryKind::Blob, end),
            entry("start", EntryKind::Blob, start),
        ],
    );
    assert_eq!(some.to_string(), LINES_SOME_OID);
    let lines = write_tree(&store, vec![entry("some", EntryKind::Tree, some)]);
    assert_eq!(lines.to_string(), LINES_OID);

    let path = write_blob(&store, b"file.txt");
    assert_eq!(path.to_string(), PATH_OID);

    let root = write_tree(
        &store,
        vec![
            entry("blob", EntryKind::Blob, blob_entry),
            entry("commit", EntryKind::Blob, commit_entry),
            entry("content", EntryKind::Blob, content),
            entry("context", EntryKind::Blob, context),
            entry("lines", EntryKind::Tree, lines),
            entry("path", EntryKind::Blob, path),
        ],
    );
    (root, store)
}

/// Item 1 + 2 of the fixture contract: every reconstructed object's oid —
/// including the root's — matches the value the current code produces.
/// Content addressing means a single corrupted byte anywhere above fails
/// this assertion (or one of `build_fixture`'s own, reached first).
#[test]
fn reconstructing_the_fixture_reproduces_every_recorded_oid() {
    let (root, _store) = build_fixture();
    assert_eq!(root.to_string(), ROOT);
}

/// Item 3: `Binding::deserialize` decodes the fixture as
/// `Binding::Position`, recovering exactly the `Anchor` the current stored
/// format has always encoded.
#[test]
fn the_fixture_deserializes_as_a_position_binding() {
    let (root, store) = build_fixture();

    let binding = Binding::deserialize(&root, &store).expect("deserialize");
    let Binding::Position(anchor) = binding else {
        panic!("the fixture must decode as Binding::Position");
    };
    assert_eq!(anchor.path, "file.txt");
    assert_eq!(anchor.lines, Some(LineRange { start: 3, end: 4 }));
    assert_eq!(anchor.content, numbered(1..=10).into_bytes());
    assert_eq!(anchor.context, numbered(1..=7).into_bytes());
    assert_eq!(anchor.blob().to_string(), CONTENT_OID);
    assert_eq!(anchor.commit().to_string(), COMMIT_ENTRY_RAW);
}

/// Item 4: re-encoding the decoded binding into a fresh store reproduces
/// the fixture's root oid exactly — the existing anchor storage format is
/// unchanged, byte for byte, now that it decodes through `Binding`.
#[test]
fn re_encoding_reproduces_the_fixture_root_byte_for_byte() {
    let (root, store) = build_fixture();
    let binding = Binding::deserialize(&root, &store).expect("deserialize");

    let fresh = ObjectStore::default();
    let re_root = binding.serialize_into(&fresh).expect("serialize");
    assert_eq!(re_root.to_string(), ROOT);
}
