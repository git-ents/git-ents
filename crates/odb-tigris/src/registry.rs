//! [`PackRegistry`]: the only source of truth `odb-tigris` consults for
//! which packs exist and are live (`docs/scale-out.adoc`, "Reachability":
//! "nothing may traverse Tigris object-by-object"). `read`/`contains` walk
//! [`PackRegistry::list`]'s result and nothing else — never a bucket listing
//! call, which the [`crate::transport::BlobTransport`] trait doesn't even
//! expose.
//!
//! [`memory::InMemoryRegistry`] is the in-process stand-in for tests and
//! conformance; the Postgres-backed implementation lives in
//! `refstore-postgres` (extending its `git_ents_pack_registry` table) rather
//! than here, so this crate never needs a `tokio-postgres` dependency of its
//! own — it depends only on the trait.

pub mod memory;

use git_backend::Result;

/// Opaque identifier for one registered pack, unique within a repo. Chosen
/// by whoever calls [`PackRegistry::record`] (in practice, the same id
/// [`crate::OdbTigris::stage_pack`] used for its quarantine key prefix).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PackId(String);

impl PackId {
    /// Wrap a backend-chosen opaque token as a `PackId`.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as a `&str`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PackId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One promoted pack, as recorded in the registry: enough to fetch its
/// index and data from the bucket, scoped to the repo it belongs to
/// (`docs/scale-out.adoc`, rule 7: "namespace per repo").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackRecord {
    /// This pack's id.
    pub id: PackId,
    /// The repo this pack belongs to.
    pub repo_id: String,
    /// The bucket key holding the pack's object data.
    pub pack_key: String,
    /// The bucket key holding the pack's `.idx`.
    pub idx_key: String,
    /// The number of objects in the pack, if known. Optional: informational
    /// only, never relied on for correctness (a stale or absent count never
    /// changes what `read`/`contains` return, since those consult the idx
    /// itself).
    pub object_count: Option<u64>,
}

/// Registry of promoted packs: the commit point [`crate::OdbTigris::promote`]
/// writes to, and the only thing [`crate::OdbTigris::read`] and
/// [`crate::OdbTigris::contains`] consult to learn which packs exist.
pub trait PackRegistry: Send + Sync {
    /// Record `record` as promoted and live. Called once per pack, after its
    /// bytes are durably in the bucket at the live keys `record` names.
    ///
    /// # Errors
    ///
    /// Returns an error if the record cannot be durably written.
    fn record(&self, record: PackRecord) -> Result<()>;

    /// All packs currently registered for `repo_id`, in no particular order.
    ///
    /// # Errors
    ///
    /// Returns an error if the registry cannot be read.
    fn list(&self, repo_id: &str) -> Result<Vec<PackRecord>>;

    /// Remove a pack from the registry (maintenance/GC use only — no code
    /// in this crate calls it on the read path). Not an error if `id` is
    /// already absent.
    ///
    /// # Errors
    ///
    /// Returns an error if the registry cannot be written.
    fn delete(&self, repo_id: &str, id: &PackId) -> Result<()>;
}
