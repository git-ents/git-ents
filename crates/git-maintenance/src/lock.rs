//! Per-repo maintenance serialization (`docs/scale-out.adoc`, WS9:
//! "Per-repo background effects serialized by advisory lock").
//!
//! [`run_exclusive`] wraps a *whole* maintenance run in one
//! [`MaintenanceLock`] acquisition, so two dispatchers (or a dispatcher
//! and an operator's manual run) can never double-run one repository: the
//! second acquirer skips — maintenance is periodic and idempotent, so
//! "skip and let the next trigger retry" beats blocking a dispatcher
//! thread on another machine's run.
//!
//! Two implementations, one per deployment shape:
//! - [`FileMaintenanceLock`] — an OS advisory file lock (`flock`-style,
//!   via `std::fs::File::try_lock`) beside the local bare repository.
//! - [`PgMaintenanceLock`] — a Postgres session advisory lock keyed by
//!   repo id, for Postgres-backed deployments where the contending
//!   dispatchers are on different machines (see
//!   `refstore_postgres::PostgresRefStore::try_maintenance_lock`).

use std::path::{Path, PathBuf};

use crate::Result;

/// Holds a per-repo maintenance lock; released on drop. The release action
/// is captured as a closure so file and Postgres guards share one type.
pub struct MaintenanceGuard<'a> {
    release: Option<Box<dyn FnOnce() + 'a>>,
}

impl Drop for MaintenanceGuard<'_> {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            release();
        }
    }
}

/// A per-repo advisory lock a maintenance run holds for its whole
/// duration.
pub trait MaintenanceLock {
    /// Try to take the lock: `Some(guard)` when this caller now holds it,
    /// `None` when another maintenance run does (the caller should skip).
    ///
    /// # Errors
    ///
    /// Returns an error if the locking mechanism itself fails — never for
    /// mere contention, which is the `None` case.
    fn try_acquire(&self) -> Result<Option<MaintenanceGuard<'_>>>;
}

/// Run `work` under `lock`, holding it for the whole run. `Ok(None)` means
/// another run holds the lock and this one was skipped.
///
/// # Errors
///
/// Returns an error if acquiring fails or `work` fails.
pub fn run_exclusive<T>(
    lock: &dyn MaintenanceLock,
    work: impl FnOnce() -> Result<T>,
) -> Result<Option<T>> {
    let Some(guard) = lock.try_acquire()? else {
        return Ok(None);
    };
    let outcome = work()?;
    drop(guard);
    Ok(Some(outcome))
}

/// [`MaintenanceLock`] over an OS advisory file lock — the local
/// deployment's serializer, correct across processes on one machine.
pub struct FileMaintenanceLock {
    path: PathBuf,
}

impl FileMaintenanceLock {
    /// A lock at `path` (created if absent; its content is never read).
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The conventional lock for the bare repository at `repo`:
    /// `<repo>/maintenance.lock`.
    #[must_use]
    pub fn for_repo(repo: &Path) -> Self {
        Self::new(repo.join("maintenance.lock"))
    }
}

impl MaintenanceLock for FileMaintenanceLock {
    fn try_acquire(&self) -> Result<Option<MaintenanceGuard<'_>>> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&self.path)?;
        match file.try_lock() {
            // Dropping the file both unlocks and closes it.
            Ok(()) => Ok(Some(MaintenanceGuard {
                release: Some(Box::new(move || drop(file))),
            })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
        }
    }
}

/// [`MaintenanceLock`] over a Postgres session advisory lock keyed by the
/// store's repo id — the cloud deployment's serializer, correct across
/// machines because the lock lives in the one Postgres primary every
/// dispatcher already talks to.
pub struct PgMaintenanceLock<'a> {
    store: &'a refstore_postgres::PostgresRefStore,
}

impl<'a> PgMaintenanceLock<'a> {
    /// Lock through `store`'s connection (session advisory locks are held
    /// by the session — this store's connection — and released on
    /// [`MaintenanceGuard`] drop or session death, so a crashed
    /// maintenance run never wedges the repo).
    #[must_use]
    pub fn new(store: &'a refstore_postgres::PostgresRefStore) -> Self {
        Self { store }
    }
}

impl MaintenanceLock for PgMaintenanceLock<'_> {
    fn try_acquire(&self) -> Result<Option<MaintenanceGuard<'_>>> {
        if !self.store.try_maintenance_lock()? {
            return Ok(None);
        }
        let store = self.store;
        Ok(Some(MaintenanceGuard {
            release: Some(Box::new(move || {
                // Session death releases the lock anyway; a failed explicit
                // unlock is not worth panicking a Drop over.
                let _ignored = store.unlock_maintenance();
            })),
        }))
    }
}
