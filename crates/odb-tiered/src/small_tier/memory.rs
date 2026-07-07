//! [`InMemorySmallTier`]: an in-process [`SmallObjectTier`], used by tests
//! and by the conformance instantiation
//! (`crates/odb-tiered/tests/conformance.rs`).

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use git_backend::{Object, Result};
use gix_hash::ObjectId;

use super::{SmallObjectTier, SmallStageId};

/// One staged-but-unpromoted batch.
struct Staged {
    repo_id: String,
    objects: Vec<(ObjectId, Object)>,
}

/// A [`SmallObjectTier`] held entirely in memory, scoped to one process.
#[derive(Default)]
pub struct InMemorySmallTier {
    promoted: Mutex<HashMap<(String, ObjectId), Object>>,
    staged: Mutex<HashMap<SmallStageId, Staged>>,
}

impl InMemorySmallTier {
    /// An empty tier.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl SmallObjectTier for InMemorySmallTier {
    fn read(&self, repo_id: &str, id: ObjectId) -> Result<Option<Object>> {
        Ok(lock(&self.promoted).get(&(repo_id.to_owned(), id)).cloned())
    }

    fn contains(&self, repo_id: &str, id: ObjectId) -> Result<bool> {
        Ok(lock(&self.promoted).contains_key(&(repo_id.to_owned(), id)))
    }

    fn stage(&self, repo_id: &str, objects: Vec<(ObjectId, Object)>) -> Result<SmallStageId> {
        let id = SmallStageId::new(uuid::Uuid::new_v4().to_string());
        lock(&self.staged).insert(
            id.clone(),
            Staged {
                repo_id: repo_id.to_owned(),
                objects,
            },
        );
        Ok(id)
    }

    fn promote(&self, id: SmallStageId) -> Result<()> {
        let Staged { repo_id, objects } = lock(&self.staged).remove(&id).ok_or_else(|| {
            git_backend::Error::ObjectStore(format!("unknown small-tier stage {id}"))
        })?;
        let mut promoted = lock(&self.promoted);
        for (oid, object) in objects {
            promoted.insert((repo_id.clone(), oid), object);
        }
        Ok(())
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
