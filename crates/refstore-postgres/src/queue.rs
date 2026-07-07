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

/// One row claimed off the effect queue by [`PostgresRefStore::claim_effects`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedEffect {
    /// The claimed row's id, needed to later call
    /// [`PostgresRefStore::complete_effect`].
    pub id: EffectId,
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
