//! [`RefStore`] over gitoxide loose refs and packed-refs — the local
//! default backend (`docs/scale-out.adoc`, "RefStore").
//!
//! Atomic multi-ref compare-and-swap is gitoxide's own ref transaction
//! (`Repository::edit_references`): every edit's precondition is checked
//! against the store's locked-in-place state, and either the whole batch
//! applies or none of it does. `log` reads the reflog gitoxide already
//! writes; `watch` is a minimal best-effort poller (see [`watch`]), since
//! local loose refs have no push notification channel to hook into.

mod watch;

use std::path::Path;
use std::sync::{Mutex, PoisonError};

use git_backend::{
    Error, Expected, RefEdit as BackendRefEdit, RefEventStream, RefIter, RefLogEntry, RefLogIter,
    RefName, RefStore, Result, TxOutcome,
};
use gix::refs::FullName;
use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit as GixRefEdit, RefLog};
use gix_hash::ObjectId;

/// The message every ref transaction's reflog entry carries. Fixed, like
/// `git_store`'s commit identity, so a write through this backend is
/// self-contained.
const LOG_MESSAGE: &str = "git-backend: transaction";

/// [`RefStore`] over a gitoxide repository's loose refs and packed-refs.
///
/// The [`gix::Repository`] handle is held behind a [`Mutex`] rather than as
/// a bare field: its internal object-access caches use interior mutability
/// that isn't `Sync`, while [`RefStore`] must be — application code holds a
/// backend behind an `Arc` and shares it across threads.
pub struct FilesRefStore {
    repo: Mutex<gix::Repository>,
}

impl FilesRefStore {
    /// Open the ref store for the repository at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        let repo = gix::open(path).map_err(|error| Error::RefStore(error.to_string()))?;
        Ok(Self {
            repo: Mutex::new(repo),
        })
    }

    /// Lock the underlying repository handle, recovering from a poisoned
    /// lock rather than panicking.
    fn repo(&self) -> std::sync::MutexGuard<'_, gix::Repository> {
        self.repo.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl RefStore for FilesRefStore {
    fn get(&self, name: &RefName) -> Result<Option<ObjectId>> {
        match self
            .repo()
            .try_find_reference(name.as_str())
            .map_err(|error| Error::RefStore(error.to_string()))?
        {
            Some(mut reference) => {
                let id = reference
                    .peel_to_id()
                    .map_err(|error| Error::RefStore(error.to_string()))?;
                Ok(Some(id.detach()))
            }
            None => Ok(None),
        }
    }

    fn iter_prefix(&self, prefix: &RefName) -> Result<RefIter> {
        let repo = self.repo();
        let platform = repo
            .references()
            .map_err(|error| Error::RefStore(error.to_string()))?;
        let iter = platform
            .prefixed(prefix.as_str())
            .map_err(|error| Error::RefStore(error.to_string()))?;
        let mut out = Vec::new();
        for reference in iter {
            let mut reference = reference.map_err(|error| Error::RefStore(error.to_string()))?;
            let name = RefName::new(reference.name().as_bstr().to_string());
            let oid = reference
                .peel_to_id()
                .map_err(|error| Error::RefStore(error.to_string()))?
                .detach();
            out.push(Ok((name, oid)));
        }
        Ok(RefIter::new(out.into_iter()))
    }

    fn transaction(&self, edits: &[BackendRefEdit]) -> Result<TxOutcome> {
        let mut gix_edits = Vec::with_capacity(edits.len());
        for edit in edits {
            gix_edits.push(to_gix_edit(edit)?);
        }
        match self.repo().edit_references(gix_edits) {
            Ok(_applied) => Ok(TxOutcome::Applied),
            Err(error) => match rejected_name(&error) {
                Some(name) => Ok(TxOutcome::Rejected { name }),
                None => Err(Error::RefStore(error.to_string())),
            },
        }
    }

    fn watch(&self, prefix: &RefName) -> Result<RefEventStream> {
        watch::spawn(
            self.repo().git_dir().to_path_buf(),
            prefix.as_str().to_owned(),
        )
    }

    fn log(&self, name: &RefName) -> Result<RefLogIter> {
        let repo = self.repo();
        let Some(reference) = repo
            .try_find_reference(name.as_str())
            .map_err(|error| Error::RefStore(error.to_string()))?
        else {
            return Ok(RefLogIter::new(std::iter::empty()));
        };
        let mut platform = reference.log_iter();
        let mut entries = Vec::new();
        if let Some(iter) = platform
            .all()
            .map_err(|error| Error::RefStore(error.to_string()))?
        {
            for line in iter {
                let line = line.map_err(|error| Error::RefStore(error.to_string()))?;
                let old = line.previous_oid();
                let new = line.new_oid();
                let seconds = line
                    .signature
                    .time()
                    .map_err(|error| Error::RefStore(error.to_string()))?
                    .seconds;
                entries.push(RefLogEntry {
                    old: (!old.is_null()).then_some(old),
                    new: (!new.is_null()).then_some(new),
                    message: line.message.to_string(),
                    seconds: u64::try_from(seconds).unwrap_or(0),
                });
            }
        }
        // The forward iterator yields oldest first; the trait promises
        // most-recent-first.
        entries.reverse();
        Ok(RefLogIter::new(entries.into_iter().map(Ok)))
    }
}

/// Convert one backend-agnostic [`BackendRefEdit`] into gitoxide's own
/// transaction edit type.
fn to_gix_edit(edit: &BackendRefEdit) -> Result<GixRefEdit> {
    let name: FullName =
        edit.name
            .as_str()
            .try_into()
            .map_err(|error: gix::validate::reference::name::Error| {
                Error::RefStore(error.to_string())
            })?;
    let change = match edit.new {
        Some(oid) => Change::Update {
            log: LogChange {
                mode: RefLog::AndReference,
                force_create_reflog: false,
                message: LOG_MESSAGE.into(),
            },
            expected: to_previous_value(&edit.expected),
            new: gix::refs::Target::Object(oid),
        },
        None => Change::Delete {
            expected: to_previous_value(&edit.expected),
            log: RefLog::AndReference,
        },
    };
    Ok(GixRefEdit {
        change,
        name,
        deref: false,
    })
}

/// Map a backend-agnostic [`Expected`] precondition onto gitoxide's own
/// [`PreviousValue`].
fn to_previous_value(expected: &Expected) -> PreviousValue {
    match expected {
        Expected::Any => PreviousValue::Any,
        Expected::MustNotExist => PreviousValue::MustNotExist,
        Expected::MustExistAndMatch(oid) => {
            PreviousValue::MustExistAndMatch(gix::refs::Target::Object(*oid))
        }
    }
}

/// The ref name a rejected transaction's compare-and-swap precondition
/// failed on, or `None` when `error` is not a CAS mismatch (some other
/// failure — a lock timeout, an I/O error — that should propagate as
/// `Err`, not `Ok(TxOutcome::Rejected)`).
fn rejected_name(error: &gix::reference::edit::Error) -> Option<RefName> {
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
    Some(RefName::new(full_name.to_string()))
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::let_underscore_must_use,
        reason = "unit test"
    )]

    use git_backend::{Expected, RefEdit, RefName, RefStore as _, TxOutcome};
    use git_store::test_support::{commit_all, head, repo};

    use super::FilesRefStore;

    /// The branch `HEAD` points at right after `git init`, whatever
    /// `init.defaultBranch` resolves to in this environment (`main`,
    /// `master`, ...).
    fn current_branch_ref(dir: &std::path::Path) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["symbolic-ref", "HEAD"])
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    #[test]
    fn get_returns_none_for_an_absent_ref() {
        let dir = repo();
        let store = FilesRefStore::open(dir.path()).unwrap();
        assert_eq!(store.get(&RefName::new("refs/heads/nope")).unwrap(), None);
    }

    #[test]
    fn get_resolves_an_existing_ref() {
        let dir = repo();
        std::fs::write(dir.path().join("file"), b"content").unwrap();
        commit_all(dir.path(), "first");
        let store = FilesRefStore::open(dir.path()).unwrap();
        let expected = gix_hash::ObjectId::from_hex(head(dir.path()).as_bytes()).unwrap();
        let branch_ref = current_branch_ref(dir.path());
        assert_eq!(
            store.get(&RefName::new(branch_ref)).unwrap(),
            Some(expected)
        );
    }

    #[test]
    fn transaction_creates_a_ref_then_rejects_a_stale_cas() {
        let dir = repo();
        std::fs::write(dir.path().join("file"), b"content").unwrap();
        commit_all(dir.path(), "first");
        let store = FilesRefStore::open(dir.path()).unwrap();
        let first_commit = gix_hash::ObjectId::from_hex(head(dir.path()).as_bytes()).unwrap();

        let create = RefEdit {
            name: RefName::new("refs/heads/topic"),
            expected: Expected::MustNotExist,
            new: Some(first_commit),
        };
        assert_eq!(store.transaction(&[create]).unwrap(), TxOutcome::Applied);

        // A second, different commit than the ref's current value.
        std::fs::write(dir.path().join("file"), b"more content").unwrap();
        commit_all(dir.path(), "second");
        let second_commit = gix_hash::ObjectId::from_hex(head(dir.path()).as_bytes()).unwrap();
        assert_ne!(first_commit, second_commit);

        // Re-asserting must-not-exist while pointing the ref at a different
        // value than what's already there is a CAS mismatch, reported as
        // `Rejected`, not an `Err`.
        let recreate = RefEdit {
            name: RefName::new("refs/heads/topic"),
            expected: Expected::MustNotExist,
            new: Some(second_commit),
        };
        let outcome = store.transaction(&[recreate]).unwrap();
        assert_eq!(
            outcome,
            TxOutcome::Rejected {
                name: RefName::new("refs/heads/topic")
            }
        );
    }

    #[test]
    fn transaction_is_all_or_nothing_across_multiple_edits() {
        let dir = repo();
        std::fs::write(dir.path().join("file"), b"content").unwrap();
        commit_all(dir.path(), "first");
        let store = FilesRefStore::open(dir.path()).unwrap();
        let commit = gix_hash::ObjectId::from_hex(head(dir.path()).as_bytes()).unwrap();

        // The second edit's precondition already fails (the ref doesn't
        // exist yet), so neither edit should apply.
        let edits = [
            RefEdit {
                name: RefName::new("refs/heads/a"),
                expected: Expected::MustNotExist,
                new: Some(commit),
            },
            RefEdit {
                name: RefName::new("refs/heads/b"),
                expected: Expected::MustExistAndMatch(commit),
                new: Some(commit),
            },
        ];
        let outcome = store.transaction(&edits).unwrap();
        assert!(matches!(outcome, TxOutcome::Rejected { .. }));
        assert_eq!(store.get(&RefName::new("refs/heads/a")).unwrap(), None);
    }

    #[test]
    fn iter_prefix_lists_matching_refs() {
        let dir = repo();
        std::fs::write(dir.path().join("file"), b"content").unwrap();
        commit_all(dir.path(), "first");
        let store = FilesRefStore::open(dir.path()).unwrap();
        let commit = gix_hash::ObjectId::from_hex(head(dir.path()).as_bytes()).unwrap();
        store
            .transaction(&[RefEdit {
                name: RefName::new("refs/meta/thing"),
                expected: Expected::MustNotExist,
                new: Some(commit),
            }])
            .unwrap();

        let names: Vec<String> = store
            .iter_prefix(&RefName::new("refs/meta/"))
            .unwrap()
            .map(|item| item.unwrap().0.as_str().to_owned())
            .collect();
        assert_eq!(names, vec!["refs/meta/thing".to_owned()]);
    }

    #[test]
    fn log_reads_back_the_transaction_message() {
        let dir = repo();
        std::fs::write(dir.path().join("file"), b"content").unwrap();
        commit_all(dir.path(), "first");
        let store = FilesRefStore::open(dir.path()).unwrap();
        let commit = gix_hash::ObjectId::from_hex(head(dir.path()).as_bytes()).unwrap();
        let name = RefName::new("refs/heads/logged");
        store
            .transaction(&[RefEdit {
                name: name.clone(),
                expected: Expected::MustNotExist,
                new: Some(commit),
            }])
            .unwrap();

        let entries: Vec<_> = store.log(&name).unwrap().map(Result::unwrap).collect();
        assert_eq!(entries.len(), 1);
        let entry = entries.first().unwrap();
        assert_eq!(entry.new, Some(commit));
        assert_eq!(entry.old, None);
    }

    #[test]
    fn watch_wakes_up_on_a_ref_change() {
        let dir = repo();
        std::fs::write(dir.path().join("file"), b"content").unwrap();
        commit_all(dir.path(), "first");
        let store = FilesRefStore::open(dir.path()).unwrap();
        let commit = gix_hash::ObjectId::from_hex(head(dir.path()).as_bytes()).unwrap();

        let watcher = store.watch(&RefName::new("refs/")).unwrap();
        // Give the poller time to take its first fingerprint before the
        // write below, so the write is guaranteed to land after it.
        std::thread::sleep(std::time::Duration::from_millis(200));
        store
            .transaction(&[RefEdit {
                name: RefName::new("refs/heads/watched"),
                expected: Expected::MustNotExist,
                new: Some(commit),
            }])
            .unwrap();
        assert!(
            watcher
                .recv_timeout(std::time::Duration::from_secs(5))
                .is_some(),
            "expected a wakeup hint after a ref change"
        );
    }
}
