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

/// Which reachability accelerator an [`ArtifactRecord`] holds — see
/// `git-reachability` (`docs/scale-out.adoc`, "Reachability" / WS6) for the
/// binary formats themselves. Named here, rather than in `git-reachability`,
/// because the registry (this trait) is the thing both that crate and its
/// Postgres implementation (`refstore-postgres`) need to agree on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactKind {
    /// A serialized commit graph: OID -> (tree, parents, generation).
    CommitGraph,
    /// A reachable-object-set snapshot for one tip-frontier.
    ReachableSet,
}

impl ArtifactKind {
    /// A stable string form, used as the on-disk/column discriminator by
    /// every [`PackRegistry`] implementation.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::CommitGraph => "commit-graph",
            Self::ReachableSet => "reachable-set",
        }
    }

    /// Parse [`Self::as_str`]'s output back, or `None` for anything else —
    /// forward-compatible with a future kind an older reader doesn't know.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "commit-graph" => Some(Self::CommitGraph),
            "reachable-set" => Some(Self::ReachableSet),
            _ => None,
        }
    }
}

/// One reachability artifact registered for a repo: enough to fetch its
/// bytes from the bucket. A repo has at most one live artifact per
/// [`ArtifactKind`] — regenerating (`git-reachability`'s maintenance effect)
/// overwrites it, rather than accumulating snapshots, so lookup is by
/// `(repo_id, kind)` alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactRecord {
    /// The repo this artifact belongs to.
    pub repo_id: String,
    /// Which accelerator this artifact holds.
    pub kind: ArtifactKind,
    /// The bucket key holding the artifact's bytes.
    pub key: String,
}

/// Registry of promoted packs: the commit point [`crate::OdbTigris::promote`]
/// writes to, and the only thing [`crate::OdbTigris::read`] and
/// [`crate::OdbTigris::contains`] consult to learn which packs exist.
///
/// Also the discovery point for reachability artifacts (`docs/
/// scale-out.adoc`, "Reachability": "stored beside packs, tracked in the
/// pack registry") — a minimal extension over the pack-only shape WS5
/// introduced, since both are "what has this repo got, and where" lookups
/// against the same store.
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

    /// Record `record` as `repo_id`'s current artifact of its kind,
    /// replacing whatever was previously registered for that
    /// `(repo_id, kind)` pair.
    ///
    /// # Errors
    ///
    /// Returns an error if the record cannot be durably written.
    fn record_artifact(&self, record: ArtifactRecord) -> Result<()>;

    /// `repo_id`'s current artifact of `kind`, or `None` if it has never
    /// been generated — the "absent artifact" case every consumer must
    /// degrade gracefully from (`docs/scale-out.adoc`, "Reachability").
    ///
    /// # Errors
    ///
    /// Returns an error if the registry cannot be read.
    fn get_artifact(&self, repo_id: &str, kind: ArtifactKind) -> Result<Option<ArtifactRecord>>;

    /// Remove `repo_id`'s artifact of `kind`, if any. Not an error if
    /// already absent.
    ///
    /// # Errors
    ///
    /// Returns an error if the registry cannot be written.
    fn delete_artifact(&self, repo_id: &str, kind: ArtifactKind) -> Result<()>;
}
