//! The vocabulary of a [`crate::RefStore::transaction`] call: what a
//! [`RefEdit`] expects a ref to hold, what a batch of them can do
//! atomically, and how the store reports which one failed.

use gix::refs::FullName;
use gix_hash::ObjectId;

/// The compare-and-swap precondition a [`RefEdit`] requires of a ref's
/// current value before the edit is allowed to apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expected {
    /// No requirement: set unconditionally.
    Any,
    /// The ref must not currently exist.
    MustNotExist,
    /// The ref must currently exist and equal the given object id.
    MustExistAndMatch(ObjectId),
}

/// One ref's half of a [`crate::RefStore::transaction`] batch: what `name`
/// is expected to hold, and what it should become. `new: None` deletes the
/// ref.
///
/// # Examples
///
/// ```
/// use gix_hash::ObjectId;
/// use gix_ref_store::{Expected, RefEdit};
///
/// let oid = ObjectId::null(gix_hash::Kind::Sha1);
/// let edit = RefEdit {
///     name: "refs/meta/config".try_into().expect("valid refname"),
///     expected: Expected::MustNotExist,
///     new: Some(oid),
/// };
/// assert_eq!(edit.new, Some(oid));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefEdit {
    /// The ref this edit applies to.
    pub name: FullName,
    /// The compare-and-swap precondition checked against `name`'s current
    /// value before the edit applies.
    pub expected: Expected,
    /// The value to set `name` to, or `None` to delete it.
    pub new: Option<ObjectId>,
}

/// The result of a [`crate::RefStore::transaction`] call that itself
/// completed (returned `Ok`): either every edit applied, or none did.
///
/// A `Rejected` outcome is not an [`crate::Error`] — a stale
/// compare-and-swap precondition is an expected, checkable result, not a
/// backend fault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxOutcome {
    /// Every edit in the batch applied atomically.
    Applied,
    /// The transaction did not apply: `name`'s current value did not match
    /// its edit's [`Expected`] precondition. No edit in the batch took
    /// effect — compare-and-swap is all-or-nothing, per the trait's
    /// contract.
    Rejected {
        /// The first ref whose precondition failed.
        name: FullName,
    },
}

/// An iterator over `(name, tip)` pairs from a
/// [`crate::RefStoreRead::iter_prefix`] query, wrapping whatever iterator
/// the backend produces so the trait itself stays object-safe.
pub struct RefIter(Box<dyn Iterator<Item = crate::Result<(FullName, ObjectId)>> + Send>);

impl RefIter {
    /// Wrap `iter` as a [`RefIter`].
    pub fn new(
        iter: impl Iterator<Item = crate::Result<(FullName, ObjectId)>> + Send + 'static,
    ) -> Self {
        Self(Box::new(iter))
    }
}

impl Iterator for RefIter {
    type Item = crate::Result<(FullName, ObjectId)>;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next()
    }
}

impl std::fmt::Debug for RefIter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RefIter(..)")
    }
}
