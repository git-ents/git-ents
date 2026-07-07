//! The dispatcher's queue seam: [`EffectQueue`] abstracts
//! `git_ents_effect_queue`'s claim/complete/requeue triangle so the
//! dispatcher loop is tested against an in-memory fake, with
//! [`refstore_postgres::PostgresRefStore`]'s `dispatcher_*` surface as the
//! real implementation.

use std::time::Duration;

use git_backend::Result;

/// One claimed queue row: the id [`EffectQueue::complete`] takes back, the
/// repository it belongs to (per-repo fairness accounting), and the
/// payload [`crate::job::decode`] reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedJob {
    /// The row's id.
    pub id: i64,
    /// The repository the row was enqueued for.
    pub repo: String,
    /// The enqueued payload.
    pub payload: String,
}

/// The at-least-once effect queue (`docs/scale-out.adoc`, "RefStore": the
/// queue table, not the watch channel, carries the guarantee). `claim`
/// transitions rows `enqueued → claimed` with a claimant and timestamp;
/// `complete` transitions `claimed → done`; `requeue_stale` returns claims
/// older than a timeout to `enqueued` — redelivery for a dispatcher that
/// died with claims outstanding. At-least-once, not exactly-once: a
/// redelivered row can run its effect twice, and effects are recorded per
/// commit, so the duplicate re-records the same outcome.
pub trait EffectQueue: Send + Sync {
    /// Atomically claim up to `limit` of the oldest `enqueued` rows for
    /// `claimed_by`, skipping rows whose repository is in `exclude_repos`
    /// (repositories at their fairness cap).
    fn claim(
        &self,
        claimed_by: &str,
        limit: usize,
        exclude_repos: &[String],
    ) -> Result<Vec<QueuedJob>>;

    /// Mark a claimed row done.
    fn complete(&self, id: i64) -> Result<()>;

    /// Return every claim older than `older_than` to `enqueued`, returning
    /// how many rows were requeued.
    fn requeue_stale(&self, older_than: Duration) -> Result<u64>;
}

impl EffectQueue for refstore_postgres::PostgresRefStore {
    fn claim(
        &self,
        claimed_by: &str,
        limit: usize,
        exclude_repos: &[String],
    ) -> Result<Vec<QueuedJob>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        Ok(self
            .dispatcher_claim(claimed_by, limit, exclude_repos)?
            .into_iter()
            .map(|row| QueuedJob {
                id: row.id.into(),
                repo: row.repo_id,
                payload: row.payload,
            })
            .collect())
    }

    fn complete(&self, id: i64) -> Result<()> {
        self.dispatcher_complete(id.into())
    }

    fn requeue_stale(&self, older_than: Duration) -> Result<u64> {
        self.dispatcher_requeue_stale(older_than)
    }
}
