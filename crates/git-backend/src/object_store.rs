//! [`ObjectStore`]: content-addressed object storage, staged then promoted.

use gix_hash::ObjectId;

use crate::Result;

/// A single object read back from an [`ObjectStore`]: its kind and its raw,
/// undeltified content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Object {
    /// The object's kind (blob, tree, commit, or tag).
    pub kind: gix_object::Kind,
    /// The object's raw content.
    pub data: Vec<u8>,
}

/// An incoming pack of objects, not yet indexed or validated, handed to
/// [`ObjectStore::stage_pack`]. Wraps whatever byte source the caller has —
/// a network connection, a file, an in-memory buffer — behind one type so
/// the trait stays object-safe.
pub struct PackStream(Box<dyn std::io::Read + Send>);

impl PackStream {
    /// Wrap `reader` as a [`PackStream`].
    pub fn new(reader: impl std::io::Read + Send + 'static) -> Self {
        Self(Box::new(reader))
    }
}

impl std::io::Read for PackStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

/// A handle to a pack staged in quarantine by [`ObjectStore::stage_pack`],
/// passed back to [`ObjectStore::promote`] once the ref transaction that
/// makes its objects reachable has committed.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct QuarantineId(String);

impl QuarantineId {
    /// Build a `QuarantineId` from a backend-chosen opaque token.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as a `&str`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for QuarantineId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Content-addressed object storage. Deliberately narrower than a full git
/// object database: there is no `write_loose`, because the tiered remote
/// backend (Tigris) this trait is also meant to describe cannot offer one —
/// small writes route through a small-object tier as ordinary staged
/// writes instead (see `docs/scale-out.adoc`, "ObjectStore").
///
/// # Contract
///
/// - **Staged objects are invisible to reachability walks and GC.**
///   [`stage_pack`](Self::stage_pack) places objects in quarantine; until
///   [`promote`](Self::promote) is called, [`read`](Self::read) and
///   [`contains`](Self::contains) against the promoted view must not see
///   them, and no reachability walk or collection may visit them either.
/// - **Ref transactions are the only commit point.** An object becomes
///   reachable only once the ref transaction pointing at it (or at
///   something that reaches it) has committed — `promote` makes objects
///   visible, it does not itself make them reachable.
pub trait ObjectStore: Send + Sync {
    /// Read the object `id`, erroring if it is not present in the promoted
    /// (non-quarantined) store.
    fn read(&self, id: ObjectId) -> Result<Object>;

    /// Whether `id` is present in the promoted (non-quarantined) store.
    fn contains(&self, id: ObjectId) -> Result<bool>;

    /// Index `pack` into quarantine, invisible to [`read`](Self::read) and
    /// [`contains`](Self::contains) until [`promote`](Self::promote) is
    /// called on the returned id.
    fn stage_pack(&self, pack: PackStream) -> Result<QuarantineId>;

    /// Make the pack staged under `q` visible to [`read`](Self::read) and
    /// [`contains`](Self::contains). Callers must not call this before the
    /// ref transaction that makes the pack's objects reachable has
    /// committed — see the trait's contract above.
    fn promote(&self, q: QuarantineId) -> Result<()>;
}
