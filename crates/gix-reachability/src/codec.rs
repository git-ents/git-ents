//! Hand-rolled binary (de)serialization shared by [`crate::commitgraph`] and
//! [`crate::reachable_set`] — no serde, no bincode (dependency policy):
//! length-prefixed where variable, fixed-width where not, every format
//! opening with a magic tag and a version byte so a future incompatible
//! change fails loudly (a wrong magic/version) rather than silently
//! misreading bytes.

use gix_hash::ObjectId;

use crate::{Error, Result};

/// An OID is always 20 bytes: every backend in this workspace pins SHA-1
/// (`docs/scale-out.adoc` backends all say so explicitly), so these formats
/// do too rather than carry a hash-kind byte for a case that never occurs.
const OID_LEN: usize = 20;

/// Append-only binary writer: a thin `Vec<u8>` wrapper naming the encoding
/// this module's readers expect.
#[derive(Default)]
pub struct Writer(Vec<u8>);

impl Writer {
    /// A fresh, empty writer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append `byte`.
    pub fn u8(&mut self, byte: u8) {
        self.0.push(byte);
    }

    /// Append `value` as 4 little-endian bytes.
    pub fn u32(&mut self, value: u32) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }

    /// Append `id`'s raw 20 bytes.
    pub fn oid(&mut self, id: &ObjectId) {
        self.0.extend_from_slice(id.as_slice());
    }

    /// Append `magic` verbatim, then `version` — every format's header.
    pub fn header(&mut self, magic: &[u8; 4], version: u8) {
        self.0.extend_from_slice(magic);
        self.u8(version);
    }

    /// Consume the writer, returning the bytes written so far.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

/// A cursor over a byte slice, reading the primitives [`Writer`] writes and
/// erroring — never panicking or slicing out of bounds — on truncation.
pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    /// A reader starting at the beginning of `data`.
    #[must_use]
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// The next `len` bytes, advancing past them.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Format`] if fewer than `len` bytes remain.
    pub fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(len)
            .ok_or_else(|| Error::Format("length overflow while reading artifact".to_owned()))?;
        let slice = self
            .data
            .get(self.pos..end)
            .ok_or_else(|| Error::Format("artifact truncated".to_owned()))?;
        self.pos = end;
        Ok(slice)
    }

    /// The next byte.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Format`] if the reader is at the end of the data.
    pub fn u8(&mut self) -> Result<u8> {
        let byte = self
            .take(1)?
            .first()
            .copied()
            .ok_or_else(|| Error::Format("artifact truncated reading a byte".to_owned()))?;
        Ok(byte)
    }

    /// The next 4 bytes as a little-endian `u32`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Format`] if fewer than 4 bytes remain.
    pub fn u32(&mut self) -> Result<u32> {
        let bytes: [u8; 4] = self
            .take(4)?
            .try_into()
            .map_err(|_error| Error::Format("artifact truncated reading a u32".to_owned()))?;
        Ok(u32::from_le_bytes(bytes))
    }

    /// The next 20 bytes as an [`ObjectId`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::Format`] if fewer than 20 bytes remain.
    pub fn oid(&mut self) -> Result<ObjectId> {
        let bytes = self.take(OID_LEN)?;
        ObjectId::try_from(bytes)
            .map_err(|_error| Error::Format("artifact carried a malformed object id".to_owned()))
    }

    /// Check and consume a header written by [`Writer::header`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::Format`] if the magic does not match or the version
    /// is not exactly `expected_version` — this crate does not (yet) carry
    /// more than one format version, so any mismatch is unreadable rather
    /// than an upgrade to shim.
    pub fn header(&mut self, magic: &[u8; 4], expected_version: u8) -> Result<()> {
        let got_magic = self.take(4)?;
        if got_magic != magic {
            return Err(Error::Format(format!(
                "unrecognized artifact magic {got_magic:02x?}"
            )));
        }
        let version = self.u8()?;
        if version != expected_version {
            return Err(Error::Format(format!(
                "unsupported artifact version {version} (expected {expected_version})"
            )));
        }
        Ok(())
    }

    /// Whether every byte has been consumed.
    #[must_use]
    pub fn at_end(&self) -> bool {
        self.pos >= self.data.len()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "unit test")]

    use super::*;

    #[test]
    fn round_trips_primitives() {
        let mut writer = Writer::new();
        writer.header(b"TEST", 1);
        writer.u32(42);
        let id = ObjectId::from_hex(b"0123456789abcdef0123456789abcdef01234567").unwrap();
        writer.oid(&id);
        let bytes = writer.into_bytes();

        let mut reader = Reader::new(&bytes);
        reader.header(b"TEST", 1).unwrap();
        assert_eq!(reader.u32().unwrap(), 42);
        assert_eq!(reader.oid().unwrap(), id);
        assert!(reader.at_end());
    }

    #[test]
    fn rejects_a_truncated_buffer() {
        let mut reader = Reader::new(&[1, 2, 3]);
        let _error = reader.u32().unwrap_err();
    }

    #[test]
    fn rejects_a_version_mismatch() {
        let mut writer = Writer::new();
        writer.header(b"TEST", 2);
        let bytes = writer.into_bytes();
        let mut reader = Reader::new(&bytes);
        let _error = reader.header(b"TEST", 1).unwrap_err();
    }
}
