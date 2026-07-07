//! [`FixtureOids`]: how `RefStore` property functions obtain object ids to
//! write into `RefEdit`s.
//!
//! A gitoxide-backed `RefStore` resolves a ref by reading the object it
//! targets (peeling through tags), so it needs oids of objects that
//! actually exist in *its own* backing repository — an oid from an
//! unrelated throwaway repo will not resolve. A backend that never touches
//! object storage (e.g. a Postgres-backed one, per `docs/scale-out.adoc`)
//! has no such requirement and can hand back any distinct synthetic value.
//! Each backend's conformance instantiation says which it is by
//! implementing this trait: [`crate::WithScratchRepo`] does it by
//! committing directly into the scratch repository it holds.

use gix_hash::ObjectId;

/// Supplies distinct object ids a `RefStore` property function can use as
/// `RefEdit` targets against this instance.
pub trait FixtureOids {
    /// `n` distinct object ids safe to write into a `RefEdit` against this
    /// instance.
    fn fixture_oids(&self, n: usize) -> Vec<ObjectId>;
}
