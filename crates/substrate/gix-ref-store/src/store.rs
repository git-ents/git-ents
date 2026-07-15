//! The write (CAS) half of the `RefStore` seam.

use crate::{RefEdit, RefStoreRead, Result, TxOutcome};

/// The unit of correctness for repository state: a store of named refs,
/// each pointing at an object id, updated only through atomic
/// compare-and-swap transactions.
///
/// `RefStore` extends [`RefStoreRead`] rather than duplicating its
/// methods, so any code already written against the read half keeps
/// working unchanged when handed a full store. `arch.refstore-read-cas-split`
/// is about restricting what the *gate* is handed, not about the store
/// implementation's own shape: one type legitimately implements both
/// halves, as [`crate::LooseRefStore`] does.
///
/// # Contract
///
/// Multi-ref compare-and-swap is contractual, not a capability query. A
/// backend that cannot apply an arbitrary batch of [`RefEdit`]s atomically
/// — every precondition checked against one consistent view, and either
/// every edit applies or none do — does not satisfy this trait, full stop.
///
/// # Examples
///
/// ```
/// use gix_hash::ObjectId;
/// use gix_ref_store::{Expected, LooseRefStore, RefEdit, RefStore, RefStoreRead, TxOutcome};
///
/// # fn run(dir: &std::path::Path, oid: ObjectId) -> gix_ref_store::Result<()> {
/// let store = LooseRefStore::open(dir)?;
/// let name: gix::refs::FullName = "refs/meta/config".try_into().expect("valid refname");
/// let outcome = store.transaction(&[RefEdit {
///     name: name.clone(),
///     expected: Expected::MustNotExist,
///     new: Some(oid),
/// }])?;
/// assert_eq!(outcome, TxOutcome::Applied);
/// assert_eq!(store.get(name.as_ref())?, Some(oid));
/// # Ok(())
/// # }
/// ```
// @relation(arch.refstore-read-cas-split, scope=file)
pub trait RefStore: RefStoreRead {
    /// Apply `edits` as one atomic compare-and-swap transaction: every
    /// edit's [`crate::Expected`] precondition is checked against the same
    /// consistent view of the store, and either every edit applies or none
    /// do. See the trait's contract above — this is not optional behavior
    /// a backend may approximate.
    fn transaction(&self, edits: &[RefEdit]) -> Result<TxOutcome>;
}
