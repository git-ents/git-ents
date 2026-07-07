//! Minimal Rust surface over `git_ents_effect_queue` and
//! `git_ents_op_records` (`docs/scale-out.adoc`, WS4's schema list). Neither
//! table is part of the [`git_backend::RefStore`] contract; these methods
//! exist so the schema is exercised end-to-end rather than asserted only by
//! its `CREATE TABLE` statements, and so a future dispatcher (WS7) and
//! `git-protocol` (op records) have something to call.

use git_backend::{Error, Result};
use gix_hash::ObjectId;

use crate::{PostgresRefStore, pg_err};

/// The primary key of a row in `git_ents_effect_queue`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EffectId(i64);

impl From<EffectId> for i64 {
    fn from(id: EffectId) -> Self {
        id.0
    }
}

impl From<i64> for EffectId {
    fn from(raw: i64) -> Self {
        Self(raw)
    }
}

/// One row claimed off the effect queue by [`PostgresRefStore::claim_effects`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedEffect {
    /// The claimed row's id, needed to later call
    /// [`PostgresRefStore::complete_effect`].
    pub id: EffectId,
    /// The effect payload enqueued by [`PostgresRefStore::enqueue_effect`].
    pub payload: String,
}

/// One row claimed off the effect queue by
/// [`PostgresRefStore::dispatcher_claim`]. Unlike [`ClaimedEffect`], it
/// carries its `repo_id`: the dispatcher serves every repository from one
/// loop and accounts its per-repo fairness cap by that attribution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchedEffect {
    /// The claimed row's id, needed to later call
    /// [`PostgresRefStore::dispatcher_complete`].
    pub id: EffectId,
    /// The repository the row was enqueued for.
    pub repo_id: String,
    /// The effect payload enqueued by [`PostgresRefStore::enqueue_effect`].
    pub payload: String,
}

impl PostgresRefStore {
    /// Append one effect payload to this store's repo-scoped queue in state
    /// `enqueued`, returning its id.
    ///
    /// # Errors
    ///
    /// Returns [`Error::RefStore`] if the insert fails.
    pub fn enqueue_effect(&self, payload: &str) -> Result<EffectId> {
        self.runtime
            .block_on(async {
                let client = self.client.lock().await;
                client
                    .query_one(
                        "INSERT INTO git_ents_effect_queue (repo_id, payload)
                         VALUES ($1, $2) RETURNING id",
                        &[&self.repo_id, &payload],
                    )
                    .await
            })
            .map_err(pg_err)
            .and_then(|row| row.try_get::<_, i64>(0).map(EffectId).map_err(pg_err))
    }

    /// Atomically claim up to `limit` of this store's oldest `enqueued`
    /// rows for `claimed_by`, marking them `claimed` so a concurrent
    /// dispatcher never double-claims them (`FOR UPDATE SKIP LOCKED`). This
    /// is the at-least-once queue the doc mandates: rows survive here
    /// independent of any `watch` subscriber's connection.
    ///
    /// # Errors
    ///
    /// Returns [`Error::RefStore`] if the claim query fails.
    pub fn claim_effects(&self, claimed_by: &str, limit: i64) -> Result<Vec<ClaimedEffect>> {
        let rows = self
            .runtime
            .block_on(async {
                let client = self.client.lock().await;
                client
                    .query(
                        "UPDATE git_ents_effect_queue
                         SET state = 'claimed', claimed_by = $1, claimed_at = now()
                         WHERE id IN (
                             SELECT id FROM git_ents_effect_queue
                             WHERE repo_id = $2 AND state = 'enqueued'
                             ORDER BY id
                             LIMIT $3
                             FOR UPDATE SKIP LOCKED
                         )
                         RETURNING id, payload",
                        &[&claimed_by, &self.repo_id, &limit],
                    )
                    .await
            })
            .map_err(pg_err)?;

        rows.into_iter()
            .map(|row| {
                let id: i64 = row.try_get(0).map_err(pg_err)?;
                let payload: String = row.try_get(1).map_err(pg_err)?;
                Ok(ClaimedEffect {
                    id: EffectId(id),
                    payload,
                })
            })
            .collect()
    }

    /// Mark a previously [`Self::claim_effects`]-claimed row `done`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::RefStore`] if the update fails.
    pub fn complete_effect(&self, id: EffectId) -> Result<()> {
        self.runtime
            .block_on(async {
                let client = self.client.lock().await;
                client
                    .execute(
                        "UPDATE git_ents_effect_queue SET state = 'done', done_at = now()
                         WHERE id = $1 AND repo_id = $2",
                        &[&id.0, &self.repo_id],
                    )
                    .await
            })
            .map_err(pg_err)
            .map(|_rows_affected| ())
    }

    /// Atomically claim up to `limit` of the oldest `enqueued` rows across
    /// *every* repository, skipping rows whose `repo_id` is in
    /// `exclude_repos` — the WS7 dispatcher's claim (`docs/scale-out.adoc`,
    /// "WS7 — Effects and Sprites"): one small machine drains the whole
    /// queue, and passes the repos currently at their per-repo fairness cap
    /// as the exclusion so a saturated repository's backlog never starves
    /// the rest. Same `FOR UPDATE SKIP LOCKED` discipline as
    /// [`Self::claim_effects`]; deliberately not scoped to this store's
    /// `repo_id` (see the crate-level note on the dispatcher exception).
    ///
    /// # Errors
    ///
    /// Returns [`Error::RefStore`] if the claim query fails.
    pub fn dispatcher_claim(
        &self,
        claimed_by: &str,
        limit: i64,
        exclude_repos: &[String],
    ) -> Result<Vec<DispatchedEffect>> {
        let exclude: Vec<String> = exclude_repos.to_vec();
        let rows = self
            .runtime
            .block_on(async {
                let client = self.client.lock().await;
                client
                    .query(
                        "UPDATE git_ents_effect_queue
                         SET state = 'claimed', claimed_by = $1, claimed_at = now()
                         WHERE id IN (
                             SELECT id FROM git_ents_effect_queue
                             WHERE state = 'enqueued' AND repo_id <> ALL($2)
                             ORDER BY id
                             LIMIT $3
                             FOR UPDATE SKIP LOCKED
                         )
                         RETURNING id, repo_id, payload",
                        &[&claimed_by, &exclude, &limit],
                    )
                    .await
            })
            .map_err(pg_err)?;

        rows.into_iter()
            .map(|row| {
                let id: i64 = row.try_get(0).map_err(pg_err)?;
                let repo_id: String = row.try_get(1).map_err(pg_err)?;
                let payload: String = row.try_get(2).map_err(pg_err)?;
                Ok(DispatchedEffect {
                    id: EffectId(id),
                    repo_id,
                    payload,
                })
            })
            .collect()
    }

    /// Return every `claimed` row (any repository) whose claim is older
    /// than `older_than` to `enqueued`, clearing the claimant — the
    /// redelivery half of the queue's at-least-once contract: a dispatcher
    /// that died with claims outstanding loses nothing, its rows come back
    /// once the timeout passes. Returns how many rows were requeued.
    ///
    /// # Errors
    ///
    /// Returns [`Error::RefStore`] if the update fails.
    pub fn dispatcher_requeue_stale(&self, older_than: std::time::Duration) -> Result<u64> {
        let seconds = older_than.as_secs_f64();
        self.runtime
            .block_on(async {
                let client = self.client.lock().await;
                client
                    .execute(
                        "UPDATE git_ents_effect_queue
                         SET state = 'enqueued', claimed_by = NULL, claimed_at = NULL
                         WHERE state = 'claimed'
                           AND claimed_at < now() - ($1 * interval '1 second')",
                        &[&seconds],
                    )
                    .await
            })
            .map_err(pg_err)
    }

    /// Mark a [`Self::dispatcher_claim`]-claimed row `done`, whichever
    /// repository it belongs to.
    ///
    /// # Errors
    ///
    /// Returns [`Error::RefStore`] if the update fails.
    pub fn dispatcher_complete(&self, id: EffectId) -> Result<()> {
        self.runtime
            .block_on(async {
                let client = self.client.lock().await;
                client
                    .execute(
                        "UPDATE git_ents_effect_queue SET state = 'done', done_at = now()
                         WHERE id = $1",
                        &[&id.0],
                    )
                    .await
            })
            .map_err(pg_err)
            .map(|_rows_affected| ())
    }

    /// Record one accepted push's op record OID (`docs/scale-out.adoc`,
    /// "Attested push": "Push ID = op record OID, uniformly").
    ///
    /// # Errors
    ///
    /// Returns [`Error::RefStore`] if the insert fails.
    pub fn record_op(&self, op_oid: ObjectId) -> Result<()> {
        let hex = op_oid.to_hex().to_string();
        self.runtime
            .block_on(async {
                let client = self.client.lock().await;
                client
                    .execute(
                        "INSERT INTO git_ents_op_records (repo_id, op_oid) VALUES ($1, $2)",
                        &[&self.repo_id, &hex],
                    )
                    .await
            })
            .map_err(pg_err)
            .map(|_rows_affected| ())
    }

    /// This store's recorded op record OIDs, most recent first.
    ///
    /// # Errors
    ///
    /// Returns [`Error::RefStore`] if the query fails, or if a stored OID is
    /// not valid hex (a corrupted row, never written by
    /// [`Self::record_op`]).
    pub fn op_records(&self) -> Result<Vec<ObjectId>> {
        let rows = self
            .runtime
            .block_on(async {
                let client = self.client.lock().await;
                client
                    .query(
                        "SELECT op_oid FROM git_ents_op_records
                         WHERE repo_id = $1 ORDER BY created_at DESC, id DESC",
                        &[&self.repo_id],
                    )
                    .await
            })
            .map_err(pg_err)?;

        rows.into_iter()
            .map(|row| {
                let hex: String = row.try_get(0).map_err(pg_err)?;
                ObjectId::from_hex(hex.as_bytes())
                    .map_err(|error| Error::RefStore(error.to_string()))
            })
            .collect()
    }
}
