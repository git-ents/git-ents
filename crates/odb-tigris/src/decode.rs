//! Resolving one object out of a pack over ranged reads (`docs/scale-out.adoc`,
//! WS5 / Q4).
//!
//! # Survey: what gix-pack offers
//!
//! `gix_pack::index::File::from_data` parses a `.idx` from any
//! `Deref<Target = [u8]>` (a plain `Vec<u8>` qualifies), so a fetched index
//! can be parsed straight out of memory — see [`crate::index_cache`].
//! `gix_pack::data::Entry::from_bytes` decodes a pack entry's header
//! (kind, decompressed size, and — for deltas — the base reference) from an
//! arbitrary byte slice plus the absolute pack offset it came from, and
//! `Entry::checked_base_pack_offset` turns an `OfsDelta`'s distance into a
//! bounds-checked absolute base offset. Both are exactly what's needed to
//! decode one entry from a ranged read rather than a full pack mmap.
//!
//! What gix-pack does *not* expose publicly is the rest of the decode path:
//! `gix_pack::data::File::decode_entry` (delta chain resolution) and
//! `gix_pack::data::delta::apply` (the actual copy/insert interpreter) both
//! assume a fully-mapped `data::File` addressed by absolute offset, and the
//! delta-apply primitives (`data::delta::apply`,
//! `data::delta::decode_header_size`) are `pub(crate)` — not reachable from
//! outside gix-pack at all. So this module reimplements the (small, stable,
//! documented) pack delta format itself: [`apply_delta`] is the same
//! copy/insert interpreter gix-pack's private `data::delta::apply` performs,
//! and [`decode_varint_size`] the same header-size varint it decodes:
//! neither has changed shape across pack format versions.
//!
//! # Ranged reads and growth
//!
//! Every fetch here is a bounded window, never the whole pack: entry headers
//! are probed with a small initial window that grows (geometrically, capped)
//! only if the header turns out to need more bytes than guessed, and
//! compressed entry data is fetched as `decompressed_size` plus a slack
//! margin, regrown the same way if zlib reports it needs more input. Real
//! packs' zlib streams essentially never need the regrowth path since
//! compressed size is bounded by decompressed size plus a small constant
//! overhead; the loop exists so a pathological pack degrades to a few extra
//! round trips instead of a wrong answer.
//!
//! # Delta chains
//!
//! [`resolve`] recurses on `OfsDelta`/`RefDelta` bases, bounded by
//! `max_depth` (matching git's own default `--depth` limit of 50) as a
//! defense against corrupt or adversarial packs with absurdly long chains —
//! independently of the fact that `OfsDelta` offsets are already
//! strictly-decreasing (a base is always earlier in the pack than the entry
//! that deltas against it) and so terminate on their own in any well-formed
//! pack. Binaries are stored un-delta'd at write time
//! (`docs/scale-out.adoc`, rule 5, and `crate::pack_writer`) precisely so
//! this recursion stays shallow in practice: only trees/commits/manifests
//! are ever candidates for a delta chain at all under this store's own
//! writer, and this store's `stage_pack` only ever indexes self-contained
//! packs (no thin-pack bases outside the pack — see `crate::OdbTigris`), so
//! `RefDelta` bases always resolve to an offset within the same pack.

use gix_object::Kind;
use gix_pack::data::Entry as PackEntry;
use gix_pack::data::entry::Header;

use crate::transport::BlobTransport;

/// git's own delta-depth ceiling, reused here as a recursion bound rather
/// than invented fresh.
const MAX_DELTA_DEPTH: u32 = 50;

/// Initial header probe window, generous enough for the overwhelming
/// majority of entries (type/size varint plus, for a ref-delta, a 20-byte
/// SHA-1) without ever growing.
const HEADER_PROBE_BYTES: u64 = 64;

/// Slack added to `decompressed_size` when fetching an entry's compressed
/// bytes, before any regrowth.
const DECOMPRESS_SLACK_BYTES: u64 = 32;

/// How many times a fetch window is allowed to double before giving up and
/// reporting corruption.
const MAX_FETCH_GROWTHS: u32 = 8;

fn corrupt(message: impl Into<String>) -> git_backend::Error {
    git_backend::Error::ObjectStore(format!("corrupt pack entry: {}", message.into()))
}

/// Look up the base of a `RefDelta` within the same pack, returning its
/// pack offset. Implemented by [`crate::OdbTigris`] over the pack's cached
/// index — see this module's doc comment for why a ref-delta base is always
/// in-pack for stores built by this crate.
pub trait RefDeltaResolver {
    /// Resolve `base_id` to a pack offset, or `None` if not found in this
    /// pack.
    fn resolve(&self, base_id: &gix_hash::oid) -> Option<u64>;
}

/// Fully resolve the object at `offset` in the pack at `pack_key`,
/// returning its kind and undeltified bytes.
///
/// # Errors
///
/// Returns an error if a ranged read fails, an entry header or delta stream
/// is corrupt, or the delta chain exceeds [`MAX_DELTA_DEPTH`].
pub fn resolve(
    transport: &dyn BlobTransport,
    pack_key: &str,
    offset: u64,
    hash_len: usize,
    ref_deltas: &dyn RefDeltaResolver,
) -> git_backend::Result<(Kind, Vec<u8>)> {
    resolve_at_depth(transport, pack_key, offset, hash_len, ref_deltas, 0)
}

fn resolve_at_depth(
    transport: &dyn BlobTransport,
    pack_key: &str,
    offset: u64,
    hash_len: usize,
    ref_deltas: &dyn RefDeltaResolver,
    depth: u32,
) -> git_backend::Result<(Kind, Vec<u8>)> {
    if depth > MAX_DELTA_DEPTH {
        return Err(corrupt("delta chain exceeds the maximum supported depth"));
    }

    let entry = fetch_entry_header(transport, pack_key, offset, hash_len)?;
    match entry.header {
        Header::Commit | Header::Tree | Header::Blob | Header::Tag => {
            let kind = entry
                .header
                .as_kind()
                .ok_or_else(|| corrupt("a non-delta header failed to convert to an object kind"))?;
            let data = decompress_entry(transport, pack_key, &entry)?;
            Ok((kind, data))
        }
        Header::OfsDelta { base_distance } => {
            let base_offset = entry
                .checked_base_pack_offset(base_distance)
                .ok_or_else(|| corrupt("ofs-delta base distance out of range"))?;
            let (kind, base_data) = resolve_at_depth(
                transport,
                pack_key,
                base_offset,
                hash_len,
                ref_deltas,
                depth.saturating_add(1),
            )?;
            let delta_data = decompress_entry(transport, pack_key, &entry)?;
            let target = apply_delta(&base_data, &delta_data)?;
            Ok((kind, target))
        }
        Header::RefDelta { base_id } => {
            let base_offset = ref_deltas
                .resolve(base_id.as_ref())
                .ok_or_else(|| corrupt("ref-delta base not found in this pack"))?;
            let (kind, base_data) = resolve_at_depth(
                transport,
                pack_key,
                base_offset,
                hash_len,
                ref_deltas,
                depth.saturating_add(1),
            )?;
            let delta_data = decompress_entry(transport, pack_key, &entry)?;
            let target = apply_delta(&base_data, &delta_data)?;
            Ok((kind, target))
        }
    }
}

/// Ranged-fetch and parse the entry header at `offset`, growing the fetch
/// window if the header (e.g. a long size varint, or a ref-delta's base id)
/// doesn't fit in the initial probe.
fn fetch_entry_header(
    transport: &dyn BlobTransport,
    pack_key: &str,
    offset: u64,
    hash_len: usize,
) -> git_backend::Result<PackEntry> {
    let mut window = HEADER_PROBE_BYTES;
    for _ in 0..MAX_FETCH_GROWTHS {
        let bytes = transport.get_range(pack_key, offset..offset.saturating_add(window))?;
        match PackEntry::from_bytes(&bytes, offset, hash_len) {
            Ok(entry) => return Ok(entry),
            Err(_) if (bytes.len() as u64) < window => {
                // The transport handed back less than we asked for, which
                // only happens at the tail of the pack: growing further
                // would never produce more bytes, so this is a real parse
                // failure, not a too-small window.
                return Err(corrupt("truncated entry header at end of pack"));
            }
            Err(_) => window = window.saturating_mul(2),
        }
    }
    Err(corrupt(
        "entry header did not fit within the fetch growth budget",
    ))
}

/// Ranged-fetch and decompress `entry`'s compressed data, growing the fetch
/// window if the zlib stream needs more input than the initial guess.
fn decompress_entry(
    transport: &dyn BlobTransport,
    pack_key: &str,
    entry: &PackEntry,
) -> git_backend::Result<Vec<u8>> {
    let out_len = usize::try_from(entry.decompressed_size)
        .map_err(|_size_error| corrupt("decompressed size does not fit in memory"))?;
    let mut window = entry
        .decompressed_size
        .saturating_add(DECOMPRESS_SLACK_BYTES);
    for _ in 0..MAX_FETCH_GROWTHS {
        let input = transport.get_range(
            pack_key,
            entry.data_offset..entry.data_offset.saturating_add(window),
        )?;
        let grew_to_end = (input.len() as u64) < window;
        let mut inflate = gix_features::zlib::Inflate::default();
        let mut out = vec![0u8; out_len];
        match inflate.once(&input, &mut out) {
            Ok((gix_features::zlib::Status::StreamEnd, _consumed_in, consumed_out))
                if consumed_out == out.len() =>
            {
                return Ok(out);
            }
            _ if grew_to_end => {
                return Err(corrupt("zlib stream did not end within the pack's bounds"));
            }
            _ => window = window.saturating_mul(2),
        }
    }
    Err(corrupt(
        "entry data did not decompress within the fetch growth budget",
    ))
}

/// Decode a delta header size varint (used for both the base-object-size
/// and result-object-size fields at the start of a delta stream). Same
/// encoding as gix-pack's private `data::delta::decode_header_size`.
fn decode_varint_size(d: &[u8]) -> git_backend::Result<(u64, usize)> {
    let mut shift: u32 = 0;
    let mut size: u64 = 0;
    for (consumed, &byte) in d.iter().enumerate() {
        if shift >= u64::BITS {
            return Err(corrupt("delta header size uses more bits than fit in u64"));
        }
        size |= (u64::from(byte) & 0x7f) << shift;
        shift = shift.saturating_add(7);
        if byte & 0x80 == 0 {
            return Ok((size, consumed.saturating_add(1)));
        }
    }
    Err(corrupt("delta header size is truncated"))
}

/// Apply a pack delta: `base` plus `delta`'s copy/insert instructions
/// produce the target object's bytes. Same instruction format as
/// gix-pack's private `data::delta::apply`; reimplemented here because that
/// function is `pub(crate)` in gix-pack (see this module's doc comment).
fn apply_delta(base: &[u8], delta: &[u8]) -> git_backend::Result<Vec<u8>> {
    let (base_size, consumed) = decode_varint_size(delta)?;
    if usize::try_from(base_size).ok() != Some(base.len()) {
        return Err(corrupt(
            "delta base size does not match resolved base object",
        ));
    }
    let rest = delta
        .get(consumed..)
        .ok_or_else(|| corrupt("delta is truncated after base size"))?;
    let (target_size, consumed2) = decode_varint_size(rest)?;
    let target_size = usize::try_from(target_size)
        .map_err(|_size_error| corrupt("delta target size does not fit in memory"))?;

    let mut out = Vec::with_capacity(target_size);
    let mut i = consumed.saturating_add(consumed2);
    while let Some(&cmd) = delta.get(i) {
        i = i.saturating_add(1);
        if cmd & 0b1000_0000 != 0 {
            let (ofs, size) = read_copy_operands(delta, &mut i, cmd)?;
            let end = ofs
                .checked_add(size)
                .ok_or_else(|| corrupt("delta copy range overflows"))?;
            out.extend_from_slice(
                base.get(ofs..end)
                    .ok_or_else(|| corrupt("delta copy range exceeds base object"))?,
            );
        } else if cmd == 0 {
            return Err(corrupt("delta command 0 is reserved and invalid"));
        } else {
            let size = usize::from(cmd);
            let end = i
                .checked_add(size)
                .ok_or_else(|| corrupt("delta insert range overflows"))?;
            out.extend_from_slice(
                delta
                    .get(i..end)
                    .ok_or_else(|| corrupt("delta insert data is truncated"))?,
            );
            i = end;
        }
    }
    if out.len() != target_size {
        return Err(corrupt(
            "delta instructions produced a different size than promised",
        ));
    }
    Ok(out)
}

/// Decode a copy instruction's offset/size operand bytes, per the pack
/// delta format's variable-length little-endian encoding selected by the
/// command byte's low 7 bits. Accumulates in `usize` throughout: every term
/// is a `u8` widened losslessly, so no truncating cast is ever needed.
fn read_copy_operands(delta: &[u8], i: &mut usize, cmd: u8) -> git_backend::Result<(usize, usize)> {
    let mut ofs: usize = 0;
    let mut size: usize = 0;
    let mut next = || -> git_backend::Result<usize> {
        let byte = *delta
            .get(*i)
            .ok_or_else(|| corrupt("delta copy instruction is truncated"))?;
        *i = i.saturating_add(1);
        Ok(usize::from(byte))
    };
    if cmd & 0x01 != 0 {
        ofs |= next()?;
    }
    if cmd & 0x02 != 0 {
        ofs |= next()? << 8;
    }
    if cmd & 0x04 != 0 {
        ofs |= next()? << 16;
    }
    if cmd & 0x08 != 0 {
        ofs |= next()? << 24;
    }
    if cmd & 0x10 != 0 {
        size |= next()?;
    }
    if cmd & 0x20 != 0 {
        size |= next()? << 8;
    }
    if cmd & 0x40 != 0 {
        size |= next()? << 16;
    }
    if size == 0 {
        size = 0x10000;
    }
    Ok((ofs, size))
}

/// Not part of this module's public surface, but exercised via
/// [`crate::OdbTigris`]'s own conformance and unit tests, since a
/// meaningful test here needs a real pack fixture — see
/// `crates/odb-tigris/tests/conformance.rs`.
#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        clippy::assertions_on_result_states,
        reason = "unit test"
    )]

    use super::*;

    #[test]
    fn decode_varint_size_round_trips_small_values() {
        // 200 encodes as two leb128-like bytes (continuation bit set on the
        // first): 0xC8 -> low 7 bits 0x48 with continuation, then 0x01.
        let (size, consumed) = decode_varint_size(&[0xC8, 0x01]).expect("decode");
        assert_eq!(size, 200);
        assert_eq!(consumed, 2);
    }

    #[test]
    fn apply_delta_rejects_a_base_size_mismatch() {
        // base_size varint says 5, but the supplied base is empty.
        let delta = [5u8, 0u8];
        assert!(apply_delta(&[], &delta).is_err());
    }
}
