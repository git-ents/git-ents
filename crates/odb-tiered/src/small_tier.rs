//! [`SmallObjectTier`]: the fast, hot-path store for small objects that
//! [`crate::OdbTiered`] consults before falling through to the underlying
//! (Tigris) store (`docs/scale-out.adoc`, "ObjectStore": "Typed documents
//! are tiny and hot; Tigris per-GET latency is the wrong floor for them.").
//!
//! Like [`git_backend::ObjectStore`] itself, staging is a first-class
//! concept here, not an afterthought: correctness rule 1 (causal collection
//! safety) and rule 2 (ref transactions are the only commit point) apply
//! just as much to objects that land in this tier as to ones that land in a
//! pack, so [`SmallObjectTier::stage`]d objects must stay invisible to
//! [`SmallObjectTier::read`]/[`SmallObjectTier::contains`] until
//! [`SmallObjectTier::promote`] is called — mirroring
//! [`git_backend::ObjectStore`]'s own contract exactly.

pub mod memory;

use git_backend::{Object, Result};
use gix_hash::ObjectId;

/// A handle to a batch staged by [`SmallObjectTier::stage`], passed back to
/// [`SmallObjectTier::promote`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SmallStageId(String);

impl SmallStageId {
    /// Build a `SmallStageId` from a backend-chosen opaque token.
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

impl std::fmt::Display for SmallStageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A small-object store: a blob/tree key-value store scoped per repo,
/// staged then promoted exactly like [`git_backend::ObjectStore`].
pub trait SmallObjectTier: Send + Sync {
    /// Read `id` from the promoted (non-staged) view of `repo_id`'s
    /// objects, or `None` if this tier doesn't hold it — a caller
    /// ([`crate::OdbTiered`]) falls through to the underlying store on
    /// `None`, so this is not itself an error.
    ///
    /// # Errors
    ///
    /// Returns an error if the tier cannot be read.
    fn read(&self, repo_id: &str, id: ObjectId) -> Result<Option<Object>>;

    /// Whether `id` is present in the promoted view of `repo_id`'s objects.
    ///
    /// # Errors
    ///
    /// Returns an error if the tier cannot be read.
    fn contains(&self, repo_id: &str, id: ObjectId) -> Result<bool>;

    /// Stage `objects` for `repo_id`, invisible to `read`/`contains` until
    /// [`Self::promote`] is called on the returned id.
    ///
    /// # Errors
    ///
    /// Returns an error if the batch cannot be durably staged.
    fn stage(&self, repo_id: &str, objects: Vec<(ObjectId, Object)>) -> Result<SmallStageId>;

    /// Make the batch staged under `id` visible to `read`/`contains`.
    ///
    /// # Errors
    ///
    /// Returns an error if promotion fails.
    fn promote(&self, id: SmallStageId) -> Result<()>;
}
