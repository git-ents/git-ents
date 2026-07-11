//! An in-memory [`RefStoreRead`] (and [`RefStore`]) implementation for
//! fixtures.

use std::collections::BTreeMap;
use std::sync::Mutex;

use gix::refs::{FullName, FullNameRef};
use gix_hash::ObjectId;
use gix_ref_store::{Expected, RefEdit, RefIter, RefStore, RefStoreRead, Result, TxOutcome};

/// An in-memory ref store: a name-to-oid map behind the same
/// [`RefStoreRead`] trait production code consumes.
///
/// Fixture *seeding* goes through [`MemRefStore::set`] and
/// [`MemRefStore::remove`] — deliberately bypassing compare-and-swap,
/// because setup is not a transaction under test. [`MemRefStore`] also
/// implements the real [`RefStore`] write half: a straightforward in-memory
/// CAS transaction over the same map, checked against one consistent
/// snapshot exactly per [`RefStore::transaction`]'s contract. This exists
/// so a crate orchestrating *real* writes through the seam under test
/// (`ents-receive`'s `receive`, and later `ents-sync`) has a fixture that
/// can be written through `RefStore` itself, not only seeded directly —
/// the CAS discipline a genuine backend must uphold under concurrent,
/// racing writers (lock ordering, precondition-read races) stays
/// `gix-ref-store`'s own [`gix_ref_store::LooseRefStore`] to test, via its
/// conformance suite; this type's transaction is single-threaded-simple by
/// construction (one `Mutex`; the whole batch runs under one lock
/// acquisition).
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

impl RefStore for MemRefStore {
    /// Apply `edits` as one atomic compare-and-swap transaction: every
    /// precondition is checked against the same locked snapshot, and
    /// either every edit applies or none do, per [`RefStore::transaction`]'s
    /// contract.
    fn transaction(&self, edits: &[RefEdit]) -> Result<TxOutcome> {
        let mut refs = self.locked();
        for edit in edits {
            let key = edit.name.as_bstr().to_string();
            let current = refs.get(&key).copied();
            let precondition_met = match edit.expected {
                Expected::Any => true,
                Expected::MustNotExist => current.is_none(),
                Expected::MustExistAndMatch(oid) => current == Some(oid),
            };
            if !precondition_met {
                return Ok(TxOutcome::Rejected {
                    name: edit.name.clone(),
                });
            }
        }
        for edit in edits {
            let key = edit.name.as_bstr().to_string();
            match edit.new {
                Some(oid) => {
                    refs.insert(key, oid);
                }
                None => {
                    refs.remove(&key);
                }
            }
        }
        Ok(TxOutcome::Applied)
    }
}
