//! An in-memory [`RefStoreRead`] implementation for fixtures.

use std::collections::BTreeMap;
use std::sync::Mutex;

use gix::refs::{FullName, FullNameRef};
use gix_hash::ObjectId;
use gix_ref_store::{RefIter, RefStoreRead, Result};

/// An in-memory ref store: a name-to-oid map behind the same
/// [`RefStoreRead`] trait production code consumes.
///
/// Mutation happens through [`MemRefStore::set`] and
/// [`MemRefStore::remove`] — deliberately *not* through the `RefStore`
/// write half, because fixtures seed state directly; the CAS discipline
/// itself is `gix-ref-store`'s to test.
///
/// # Examples
///
/// ```
/// use ents_testutil::MemRefStore;
/// use gix_ref_store::RefStoreRead;
///
/// let store = MemRefStore::default();
/// let name: gix::refs::FullName = "refs/heads/main".try_into().expect("valid");
/// let oid = gix_hash::ObjectId::null(gix_hash::Kind::Sha1);
///
/// store.set(name.as_ref(), oid);
/// assert_eq!(store.get(name.as_ref()).expect("readable"), Some(oid));
///
/// store.remove(name.as_ref());
/// assert_eq!(store.get(name.as_ref()).expect("readable"), None);
/// ```
#[derive(Debug, Default)]
pub struct MemRefStore {
    refs: Mutex<BTreeMap<String, ObjectId>>,
}

impl MemRefStore {
    /// Set `name` to `oid`, creating or overwriting it.
    pub fn set(&self, name: &FullNameRef, oid: ObjectId) {
        self.locked().insert(name.as_bstr().to_string(), oid);
    }

    /// Set the ref named by `name` (a full refname string) to `oid`.
    ///
    /// Panics if `name` is not a valid full refname.
    ///
    /// # Examples
    ///
    /// ```
    /// use ents_testutil::MemRefStore;
    ///
    /// let store = MemRefStore::default();
    /// store.set_str("refs/heads/main", gix_hash::ObjectId::null(gix_hash::Kind::Sha1));
    /// ```
    pub fn set_str(&self, name: &str, oid: ObjectId) {
        let name: FullName = name.try_into().expect("valid refname in fixture");
        self.set(name.as_ref(), oid);
    }

    /// Delete `name` if present.
    pub fn remove(&self, name: &FullNameRef) {
        let key = name.as_bstr().to_string();
        let _removed = self.locked().remove(&key);
    }

    /// A deep copy of this store's current refs — the fixture analogue of
    /// a fetch, for pre-flight call-site tests that evaluate against a
    /// clone's ref state.
    ///
    /// # Examples
    ///
    /// ```
    /// use ents_testutil::MemRefStore;
    /// use gix_ref_store::RefStoreRead;
    ///
    /// let store = MemRefStore::default();
    /// store.set_str("refs/heads/main", gix_hash::ObjectId::null(gix_hash::Kind::Sha1));
    ///
    /// let fetched = store.fetched_copy();
    /// let name: gix::refs::FullName = "refs/heads/main".try_into().expect("valid");
    /// assert_eq!(
    ///     fetched.get(name.as_ref()).expect("readable"),
    ///     store.get(name.as_ref()).expect("readable"),
    /// );
    /// ```
    #[must_use]
    pub fn fetched_copy(&self) -> Self {
        Self {
            refs: Mutex::new(self.locked().clone()),
        }
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, ObjectId>> {
        self.refs
            .lock()
            .expect("ref-store mutex poisoned in fixture")
    }
}

impl RefStoreRead for MemRefStore {
    fn get(&self, name: &FullNameRef) -> Result<Option<ObjectId>> {
        Ok(self.locked().get(&name.as_bstr().to_string()).copied())
    }

    fn iter_prefix(&self, prefix: &str) -> Result<RefIter> {
        let snapshot: Vec<_> = self
            .locked()
            .range(prefix.to_owned()..)
            .take_while(|(name, _)| name.starts_with(prefix))
            .map(|(name, oid)| {
                let full: FullName = name
                    .as_str()
                    .try_into()
                    .expect("only valid refnames are ever inserted");
                Ok((full, *oid))
            })
            .collect();
        Ok(RefIter::new(snapshot.into_iter()))
    }
}
