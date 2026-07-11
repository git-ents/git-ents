//! The read half of the `RefStore` seam.
//!
//! `arch.refstore-read-cas-split` requires that a consumer able to check
//! ref state never automatically gains the ability to change it. The gate
//! (`gate.adoc`) is the reason this split exists: it is a pure function
//! over ref-store reads and must be statically incapable of writing.

use gix::refs::FullNameRef;
use gix_hash::ObjectId;

use crate::{RefIter, Result};

/// The read half of a `RefStore`: everything needed to evaluate the gate
/// (`gate.adoc`) or render a UI, with no path to mutation.
///
/// A type that also supports writes implements [`crate::RefStore`], which
/// extends this trait with [`crate::RefStore::transaction`]. Code that only
/// ever needs to read — the gate above all — should be written against
/// `RefStoreRead` (or `&dyn RefStoreRead`) so it is impossible, not just
/// disciplined, for it to write.
///
/// # Examples
///
/// ```
/// use gix_ref_store::{LooseRefStore, RefStoreRead};
///
/// # fn open(dir: &std::path::Path) -> gix_ref_store::Result<()> {
/// let store = LooseRefStore::open(dir)?;
/// let read: &dyn RefStoreRead = &store;
/// let name: gix::refs::FullName = "refs/heads/does-not-exist".try_into().expect("valid refname");
/// assert_eq!(read.get(name.as_ref())?, None);
/// # Ok(())
/// # }
/// ```
// @relation(arch.refstore-read-cas-split, scope=file)
pub trait RefStoreRead: Send + Sync {
    /// The object id `name` currently points at, or `None` if `name` does
    /// not exist.
    fn get(&self, name: &FullNameRef) -> Result<Option<ObjectId>>;

    /// Every ref under `prefix` (for example `refs/meta/`), with its
    /// current tip.
    fn iter_prefix(&self, prefix: &str) -> Result<RefIter>;
}

/// Blanket impl so a `RefStoreRead` behind any indirection remains usable
/// as `RefStoreRead` itself — `&T`, `Box<T>`, and `std::sync::Arc<T>` all
/// forward transparently.
impl<T: RefStoreRead + ?Sized> RefStoreRead for &T {
    fn get(&self, name: &FullNameRef) -> Result<Option<ObjectId>> {
        (**self).get(name)
    }

    fn iter_prefix(&self, prefix: &str) -> Result<RefIter> {
        (**self).iter_prefix(prefix)
    }
}

impl<T: RefStoreRead + ?Sized> RefStoreRead for std::sync::Arc<T> {
    fn get(&self, name: &FullNameRef) -> Result<Option<ObjectId>> {
        (**self).get(name)
    }

    fn iter_prefix(&self, prefix: &str) -> Result<RefIter> {
        (**self).iter_prefix(prefix)
    }
}
