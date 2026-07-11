//! [`LooseRefStore`]: `RefStore` over gitoxide loose refs and packed-refs —
//! the local default backend (`roots.local`).
//!
//! Atomic multi-ref compare-and-swap is layered on gitoxide's own
//! in-process ref transaction (`Repository::edit_references_as`), which
//! does the actual loose-file write, reflog append, and packed-refs
//! interaction. Nothing in this module shells out to `git`.
//!
//! gitoxide's file-transaction precondition check reads a ref's current
//! value *before* acquiring that ref's lock file, then locks and writes
//! without re-verifying — safe for callers who already serialize through
//! one in-process handle, but not for two independent `gix::Repository`
//! handles (two processes, or two handles opened separately in one
//! process) racing the same ref: both can read the same stale
//! precondition before either has locked anything, and both then "win".
//! `arch.loose-cas-discipline` requires this store to write through *its
//! own* compare-and-swap discipline, so [`LooseRefStore::transaction`]
//! closes that window itself with [`STORE_LOCK_NAME`]: a lock file,
//! separate from any ref's own `.lock`, that every `transaction()` call —
//! from any handle, any process, sharing the same on-disk repository —
//! must hold for the full read-check-write sequence before gitoxide's own
//! per-ref locking ever begins.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit as GixRefEdit, RefLog};
use gix::refs::{FullName, FullNameRef, Target};
use gix_hash::ObjectId;

use crate::{Error, Expected, RefEdit, RefIter, RefStore, RefStoreRead, Result, TxOutcome};

/// The lock file name, held for the duration of every
/// [`LooseRefStore::transaction`] call, that closes the precondition
/// TOCTOU window described in this module's doc comment. Deliberately
/// distinct from any ref's own name so it can never collide with a
/// `refs/**` path gitoxide locks internally.
const STORE_LOCK_NAME: &str = "gix-ref-store.lock";

/// How long [`LooseRefStore::transaction`] waits to acquire
/// [`STORE_LOCK_NAME`] before giving up. Generous relative to how long a
/// transaction actually holds it (a handful of small file writes), so a
/// legitimate queue of waiters drains rather than spuriously failing.
const STORE_LOCK_TIMEOUT: Duration = Duration::from_secs(5);

/// The identity every transaction's reflog entry is written under.
///
/// A `RefStore` write is a plumbing-level operation, not an authored
/// change — `gate.adoc`'s tip invariant is what carries authorship for
/// meta-ref content, via the commit's own signature. The reflog identity
/// here exists only so gitoxide has somewhere to write a committer line;
/// it is deliberately fixed and independent of the local `git config`, so
/// a `LooseRefStore` never depends on `user.name`/`user.email` being set.
const REFLOG_NAME: &str = "gix-ref-store";
const REFLOG_EMAIL: &str = "ref-store@git-ents.invalid";
const REFLOG_MESSAGE: &str = "gix-ref-store: transaction";

/// [`RefStore`] over a gitoxide repository's loose refs and packed-refs.
///
/// # Examples
///
/// ```
/// use gix_ref_store::LooseRefStore;
///
/// # fn open(dir: &std::path::Path) -> gix_ref_store::Result<()> {
/// let store = LooseRefStore::open(dir)?;
/// # let _ = store;
/// # Ok(())
/// # }
/// ```
pub struct LooseRefStore {
    repo: Mutex<gix::Repository>,
    /// The repository's git directory, captured at open time so
    /// [`Self::store_lock_path`] can be computed without locking
    /// [`Self::repo`] — the store-level lock must be acquired *before* any
    /// gitoxide call touches `repo`, not while already holding it.
    git_dir: PathBuf,
}

impl LooseRefStore {
    /// Open the ref store for the repository at `path`.
    ///
    /// `path` may be a work tree or the `.git` directory itself; gitoxide
    /// resolves either the same way `git` does.
    // @relation(arch.loose-cas-discipline, scope=function)
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let repo = gix::open(path).map_err(|source| Error::Open {
            path: path.to_path_buf(),
            source: Box::new(source),
        })?;
        let git_dir = repo.git_dir().to_path_buf();
        Ok(Self {
            repo: Mutex::new(repo),
            git_dir,
        })
    }

    /// Lock the underlying repository handle, recovering from a poisoned
    /// lock rather than panicking: a panic in one caller while holding the
    /// lock must not permanently wedge every other caller sharing this
    /// store.
    fn repo(&self) -> std::sync::MutexGuard<'_, gix::Repository> {
        self.repo.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The fixed reflog identity every transaction is written under. See
    /// [`REFLOG_NAME`] for why this is not the ambient `git config`
    /// identity.
    fn committer(&self) -> gix::actor::Signature {
        gix::actor::Signature {
            name: REFLOG_NAME.into(),
            email: REFLOG_EMAIL.into(),
            time: gix::date::Time::now_local_or_utc(),
        }
    }

    /// The path of this store's own serialization lock — see this
    /// module's doc comment for why `transaction` needs one beyond
    /// whatever gitoxide locks internally.
    fn store_lock_path(&self) -> PathBuf {
        self.git_dir.join(STORE_LOCK_NAME)
    }
}

impl RefStoreRead for LooseRefStore {
    fn get(&self, name: &FullNameRef) -> Result<Option<ObjectId>> {
        let repo = self.repo();
        let Some(mut reference) = repo
            .try_find_reference(name.as_bstr())
            .map_err(|error| Error::Read(Box::new(error)))?
        else {
            return Ok(None);
        };
        let id = reference
            .follow_to_object()
            .map_err(|error| Error::Read(Box::new(error)))?;
        Ok(Some(id.detach()))
    }

    fn iter_prefix(&self, prefix: &str) -> Result<RefIter> {
        let repo = self.repo();
        let platform = repo
            .references()
            .map_err(|error| Error::Read(Box::new(error)))?;
        let iter = platform
            .prefixed(prefix)
            .map_err(|error| Error::Read(Box::new(error)))?;

        let mut out = Vec::new();
        for reference in iter {
            let mut reference = reference.map_err(Error::Read)?;
            let name = reference.name().to_owned();
            let oid = reference
                .follow_to_object()
                .map_err(|error| Error::Read(Box::new(error)))?
                .detach();
            out.push(Ok((name, oid)));
        }
        Ok(RefIter::new(out.into_iter()))
    }
}

impl RefStore for LooseRefStore {
    // @relation(arch.loose-cas-discipline, scope=function)
    fn transaction(&self, edits: &[RefEdit]) -> Result<TxOutcome> {
        // Close the precondition-read-before-lock race described in this
        // module's doc comment: no other `transaction()` call, on this
        // handle or any other handle sharing this on-disk repository, may
        // be inside its own read-check-write sequence while we are.
        let _store_lock = gix_lock::Marker::acquire_to_hold_resource(
            self.store_lock_path(),
            gix_lock::acquire::Fail::AfterDurationWithBackoff(STORE_LOCK_TIMEOUT),
            Some(self.git_dir.clone()),
        )
        .map_err(Error::StoreLock)?;

        let gix_edits: Vec<GixRefEdit> = edits.iter().map(to_gix_edit).collect();
        let committer = self.committer();
        let mut buf = gix::date::parse::TimeBuf::default();
        match self
            .repo()
            .edit_references_as(gix_edits, Some(committer.to_ref(&mut buf)))
        {
            Ok(_applied) => Ok(TxOutcome::Applied),
            Err(error) => match rejected_name(&error) {
                Some(name) => Ok(TxOutcome::Rejected { name }),
                None => Err(Error::Transaction(error)),
            },
        }
    }
}

/// Convert one backend-agnostic [`RefEdit`] into gitoxide's own
/// transaction edit type.
fn to_gix_edit(edit: &RefEdit) -> GixRefEdit {
    let change = match edit.new {
        Some(oid) => Change::Update {
            log: LogChange {
                mode: RefLog::AndReference,
                // gitoxide only auto-creates a missing reflog for
                // refs/heads/, refs/remotes/, refs/notes/, and HEAD unless
                // told otherwise; this project's refs mostly live under
                // refs/meta/*, which needs a log regardless of namespace.
                force_create_reflog: true,
                message: REFLOG_MESSAGE.into(),
            },
            expected: to_previous_value(&edit.expected),
            new: Target::Object(oid),
        },
        None => Change::Delete {
            expected: to_previous_value(&edit.expected),
            log: RefLog::AndReference,
        },
    };
    GixRefEdit {
        change,
        name: edit.name.clone(),
        deref: false,
    }
}

/// Map a backend-agnostic [`Expected`] precondition onto gitoxide's own
/// [`PreviousValue`].
fn to_previous_value(expected: &Expected) -> PreviousValue {
    match expected {
        Expected::Any => PreviousValue::Any,
        Expected::MustNotExist => PreviousValue::MustNotExist,
        Expected::MustExistAndMatch(oid) => PreviousValue::MustExistAndMatch(Target::Object(*oid)),
    }
}

/// The ref name a rejected transaction's compare-and-swap precondition
/// failed on, or `None` when `error` is not a CAS mismatch (some other
/// failure — a lock timeout, an I/O error — that should propagate as
/// `Err`, not `Ok(TxOutcome::Rejected)`).
fn rejected_name(error: &gix::reference::edit::Error) -> Option<FullName> {
    let gix::reference::edit::Error::FileTransactionPrepare(prepare_error) = error else {
        return None;
    };
    use gix::refs::file::transaction::prepare::Error as PrepareError;
    let full_name = match prepare_error {
        PrepareError::MustNotExist { full_name, .. }
        | PrepareError::MustExist { full_name, .. }
        | PrepareError::ReferenceOutOfDate { full_name, .. }
        | PrepareError::DeleteReferenceMustExist { full_name, .. } => full_name,
        _ => return None,
    };
    full_name_from_bytes(full_name.clone())
}

/// Reconstruct a validated [`FullName`] from the raw bytes a
/// `prepare::Error` variant carries.
///
/// These bytes always originated from a [`FullName`] we constructed
/// ourselves in [`to_gix_edit`] and handed to gitoxide, so re-validating
/// them can only fail if gitoxide's own transaction machinery corrupted a
/// name it was given — a backend bug, not a caller error. `None` is
/// returned rather than panicking so a hypothetical future gitoxide
/// version that reports a differently-shaped name degrades to "not
/// recognized as a CAS rejection" instead of crashing the caller.
fn full_name_from_bytes(bytes: gix::bstr::BString) -> Option<FullName> {
    FullName::try_from(bytes).ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "unit test")]

    use gix_hash::ObjectId;

    use super::LooseRefStore;
    use crate::{Expected, RefEdit, RefStore, RefStoreRead, TxOutcome};

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        gix::init(dir.path()).unwrap();
        dir
    }

    fn name(s: &str) -> gix::refs::FullName {
        s.try_into().unwrap()
    }

    fn fixture_oid(byte: u8) -> ObjectId {
        ObjectId::from_bytes_or_panic(&[byte; 20])
    }

    #[test]
    fn get_returns_none_for_an_absent_ref() {
        let dir = init_repo();
        let store = LooseRefStore::open(dir.path()).unwrap();
        assert_eq!(store.get(name("refs/heads/nope").as_ref()).unwrap(), None);
    }

    #[test]
    fn transaction_creates_a_ref_then_rejects_a_stale_cas() {
        let dir = init_repo();
        let store = LooseRefStore::open(dir.path()).unwrap();
        let first = fixture_oid(1);
        let second = fixture_oid(2);

        let create = RefEdit {
            name: name("refs/heads/topic"),
            expected: Expected::MustNotExist,
            new: Some(first),
        };
        assert_eq!(store.transaction(&[create]).unwrap(), TxOutcome::Applied);
        assert_eq!(
            store.get(name("refs/heads/topic").as_ref()).unwrap(),
            Some(first)
        );

        // Re-asserting must-not-exist while the ref already exists is a
        // CAS mismatch, reported as `Rejected`, not an `Err`.
        let recreate = RefEdit {
            name: name("refs/heads/topic"),
            expected: Expected::MustNotExist,
            new: Some(second),
        };
        let outcome = store.transaction(&[recreate]).unwrap();
        assert_eq!(
            outcome,
            TxOutcome::Rejected {
                name: name("refs/heads/topic")
            }
        );
        // The rejected edit must not have applied.
        assert_eq!(
            store.get(name("refs/heads/topic").as_ref()).unwrap(),
            Some(first)
        );
    }

    #[test]
    fn transaction_is_all_or_nothing_across_multiple_edits() {
        let dir = init_repo();
        let store = LooseRefStore::open(dir.path()).unwrap();
        let oid = fixture_oid(3);

        // The second edit's precondition already fails (the ref doesn't
        // exist yet), so neither edit should apply.
        let edits = [
            RefEdit {
                name: name("refs/heads/a"),
                expected: Expected::MustNotExist,
                new: Some(oid),
            },
            RefEdit {
                name: name("refs/heads/b"),
                expected: Expected::MustExistAndMatch(oid),
                new: Some(oid),
            },
        ];
        let outcome = store.transaction(&edits).unwrap();
        assert!(matches!(outcome, TxOutcome::Rejected { .. }));
        assert_eq!(store.get(name("refs/heads/a").as_ref()).unwrap(), None);
    }

    #[test]
    fn iter_prefix_lists_matching_refs() {
        let dir = init_repo();
        let store = LooseRefStore::open(dir.path()).unwrap();
        let oid = fixture_oid(4);
        store
            .transaction(&[RefEdit {
                name: name("refs/meta/thing"),
                expected: Expected::MustNotExist,
                new: Some(oid),
            }])
            .unwrap();
        store
            .transaction(&[RefEdit {
                name: name("refs/heads/unrelated"),
                expected: Expected::MustNotExist,
                new: Some(oid),
            }])
            .unwrap();

        let names: Vec<String> = store
            .iter_prefix("refs/meta/")
            .unwrap()
            .map(|item| item.unwrap().0.as_bstr().to_string())
            .collect();
        assert_eq!(names, vec!["refs/meta/thing".to_owned()]);
    }

    #[test]
    fn delete_removes_a_ref() {
        let dir = init_repo();
        let store = LooseRefStore::open(dir.path()).unwrap();
        let oid = fixture_oid(5);
        store
            .transaction(&[RefEdit {
                name: name("refs/meta/gone"),
                expected: Expected::MustNotExist,
                new: Some(oid),
            }])
            .unwrap();
        assert_eq!(
            store.get(name("refs/meta/gone").as_ref()).unwrap(),
            Some(oid)
        );

        let outcome = store
            .transaction(&[RefEdit {
                name: name("refs/meta/gone"),
                expected: Expected::MustExistAndMatch(oid),
                new: None,
            }])
            .unwrap();
        assert_eq!(outcome, TxOutcome::Applied);
        assert_eq!(store.get(name("refs/meta/gone").as_ref()).unwrap(), None);
    }
}
