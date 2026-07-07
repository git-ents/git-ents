//! Exercises `odb-tigris`'s ranged-read `OfsDelta` resolution
//! (`crates/odb-tigris/src/decode.rs`) against a pack this test hand-builds
//! specifically to contain a delta entry.
//!
//! The conformance suite's fixture pack (one small commit) is realistic but
//! too small for `git pack-objects` to bother delta-compressing anything,
//! so it never exercises `decode::resolve`'s delta branch. Relying on git's
//! own (unspecified, version-dependent) heuristics to *maybe* produce a
//! delta would make this test flaky, so instead this file constructs a
//! minimal, deliberately-deltified two-object pack by hand: a full blob,
//! and an `OfsDelta` entry against it, each zlib-wrapped with a trivial
//! *stored* (uncompressed) deflate block — valid per the DEFLATE spec and
//! decodable by any conforming inflate implementation, without pulling in a
//! compression crate. `gix_pack::Bundle::write_to_directory` (invoked via
//! `OdbTigris::stage_pack`) indexes this pack exactly like a real one, and
//! `OdbTigris::read` then has to reconstruct the delta target purely from
//! ranged reads to get the right answer.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    reason = "test fixture hand-building a pack from fixed, known-small lengths, not application code"
)]

use std::io::Cursor;

use git_backend::{ObjectStore as _, PackStream};
use gix_hash::{Kind as HashKind, ObjectId};
use gix_object::Kind;
use gix_pack::data::entry::Header;
use odb_tigris::OdbTigris;
use odb_tigris::registry::memory::InMemoryRegistry;
use odb_tigris::transport::fs::FsTransport;

/// The git blob object id for `data`: `sha1("blob {len}\0" + data)`, the
/// same content address `gix_pack`'s own indexer computes for our
/// hand-built pack's objects — used here only to know what to `read()`
/// back, not something the store is told.
fn blob_oid(data: &[u8]) -> ObjectId {
    let mut hasher = gix_hash::hasher(HashKind::Sha1);
    hasher.update(format!("blob {}\0", data.len()).as_bytes());
    hasher.update(data);
    hasher.try_finalize().expect("hash blob")
}

/// Adler-32, the checksum zlib appends after its deflate stream.
fn adler32(data: &[u8]) -> u32 {
    const MOD_ADLER: u32 = 65521;
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for &byte in data {
        a = (a + u32::from(byte)) % MOD_ADLER;
        b = (b + a) % MOD_ADLER;
    }
    (b << 16) | a
}

/// Wrap `data` in a valid zlib stream using a single uncompressed ("stored")
/// deflate block — no real compression, just the minimal envelope zlib
/// requires, which `gix_features::zlib::Inflate` (backed by `zlib-rs`, a
/// spec-conforming implementation) must decode like any other zlib stream.
fn zlib_store(data: &[u8]) -> Vec<u8> {
    let len = u16::try_from(data.len()).expect("test fixture data fits in one stored block");
    let mut out = vec![0x78, 0x01]; // valid zlib header (CM=8/CINFO=7, FLEVEL=fastest)
    out.push(0x01); // final stored block: BFINAL=1, BTYPE=00
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&(!len).to_le_bytes());
    out.extend_from_slice(data);
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

/// A copy instruction copying `size` bytes starting at `ofs` from the base
/// object, encoded per the pack delta format (mirrors
/// `odb_tigris`'s own `apply_delta` decoder, used here in reverse).
fn copy_op(ofs: u8, size: u8) -> Vec<u8> {
    // Only the low byte of each of ofs/size is ever needed for this test's
    // small fixture, so only bits 0x01 (ofs low byte) and 0x10 (size low
    // byte) of the command byte are ever set.
    vec![0x80 | 0x01 | 0x10, ofs, size]
}

/// An insert instruction embedding `bytes` literally (length must be 1..=127).
fn insert_op(bytes: &[u8]) -> Vec<u8> {
    let len = u8::try_from(bytes.len()).expect("test fixture insert fits one instruction");
    assert!(len > 0 && len < 128, "insert length must fit the opcode");
    let mut out = vec![len];
    out.extend_from_slice(bytes);
    out
}

/// Hand-encode a delta transforming `base` into `target`, where `target` is
/// `base`'s first `prefix_len` bytes, then `middle`, then `base`'s last
/// `suffix_len` bytes — exactly the shape this test's fixture uses.
fn build_delta(base: &[u8], prefix_len: u8, middle: &[u8], suffix_len: u8) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(u8::try_from(base.len()).expect("test base fits one varint byte"));
    let target_len = usize::from(prefix_len) + middle.len() + usize::from(suffix_len);
    out.push(u8::try_from(target_len).expect("test target fits one varint byte"));
    out.extend(copy_op(0, prefix_len));
    out.extend(insert_op(middle));
    let suffix_ofs = u8::try_from(base.len()).expect("fits u8") - suffix_len;
    out.extend(copy_op(suffix_ofs, suffix_len));
    out
}

/// Build a minimal, valid version-2 pack containing exactly two objects:
/// `base` as a full blob, then an `OfsDelta` entry against it decoding to
/// `target`.
fn build_pack(base: &[u8], target_prefix: u8, target_middle: &[u8], target_suffix: u8) -> Vec<u8> {
    let mut pack = Vec::new();
    pack.extend_from_slice(b"PACK");
    pack.extend_from_slice(&2u32.to_be_bytes()); // pack version
    pack.extend_from_slice(&2u32.to_be_bytes()); // object count

    let entry0_offset = u64::try_from(pack.len()).expect("fits u64");
    let mut header0 = Vec::new();
    Header::Blob
        .write_to(base.len() as u64, &mut header0)
        .expect("write blob header");
    pack.extend_from_slice(&header0);
    pack.extend_from_slice(&zlib_store(base));

    let entry1_offset = u64::try_from(pack.len()).expect("fits u64");
    let base_distance = entry1_offset - entry0_offset;
    let delta = build_delta(base, target_prefix, target_middle, target_suffix);
    let mut header1 = Vec::new();
    Header::OfsDelta { base_distance }
        .write_to(delta.len() as u64, &mut header1)
        .expect("write ofs-delta header");
    pack.extend_from_slice(&header1);
    pack.extend_from_slice(&zlib_store(&delta));

    let mut hasher = gix_hash::hasher(HashKind::Sha1);
    hasher.update(&pack);
    let trailer = hasher.try_finalize().expect("hash pack");
    pack.extend_from_slice(trailer.as_slice());
    pack
}

#[test]
fn resolves_an_ofs_delta_entry_via_ranged_reads() {
    let base = b"01234567890123456789".to_vec(); // 20 bytes
    let middle = b"DELTA-INSERTED-MIDDLE".to_vec();
    let (prefix_len, suffix_len) = (5u8, 5u8);
    let pack_bytes = build_pack(&base, prefix_len, &middle, suffix_len);

    let mut target = base[..usize::from(prefix_len)].to_vec();
    target.extend_from_slice(&middle);
    target.extend_from_slice(&base[base.len() - usize::from(suffix_len)..]);

    let dir = tempfile::tempdir().expect("tempdir");
    let transport = FsTransport::open(dir.path().join("bucket")).expect("open transport");
    let store = OdbTigris::new(transport, InMemoryRegistry::new(), "delta-test-repo");

    let quarantine = store
        .stage_pack(PackStream::new(Cursor::new(pack_bytes)))
        .expect("stage_pack indexes the hand-built pack");
    store.promote(quarantine).expect("promote");

    let base_object = store.read(blob_oid(&base)).expect("read base object");
    assert_eq!(base_object.kind, Kind::Blob);
    assert_eq!(base_object.data, base);

    let target_object = store
        .read(blob_oid(&target))
        .expect("read delta-reconstructed object");
    assert_eq!(target_object.kind, Kind::Blob);
    assert_eq!(
        target_object.data, target,
        "OfsDelta resolution over ranged reads must reproduce the exact target bytes"
    );
}
