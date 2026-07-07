//! [`odb_tigris::registry::PackRegistry`] for [`PostgresRefStore`]: rows in
//! `git_ents_pack_registry`, extended (see `migrations/0001_init.sql`) with
//! the columns WS5 needs beyond WS4's original minimal shape
//! (`docs/scale-out.adoc`, "ObjectStore" / WS5).
//!
//! Kept in this crate rather than `odb-tigris` itself so `odb-tigris` never
//! needs a `tokio-postgres` dependency of its own — it depends only on the
//! [`odb_tigris::registry::PackRegistry`] trait, and this crate (which
//! already owns the Postgres connection) implements it.

use git_backend::{Error, Result};
use odb_tigris::registry::{PackId, PackRecord, PackRegistry};

use crate::{PostgresRefStore, pg_err};

impl PackRegistry for PostgresRefStore {
    fn record(&self, record: PackRecord) -> Result<()> {
        self.runtime
            .block_on(async {
                let client = self.client.lock().await;
                client
                    .execute(
                        "INSERT INTO git_ents_pack_registry
                             (repo_id, pack_id, pack_key, idx_key, object_count, promoted_at)
                         VALUES ($1, $2, $3, $4, $5, now())
                         ON CONFLICT (repo_id, pack_id) DO UPDATE SET
                             pack_key = EXCLUDED.pack_key,
                             idx_key = EXCLUDED.idx_key,
                             object_count = EXCLUDED.object_count,
                             promoted_at = EXCLUDED.promoted_at",
                        &[
                            &record.repo_id,
                            &record.id.as_str(),
                            &record.pack_key,
                            &record.idx_key,
                            &record
                                .object_count
                                .map(|count| i64::try_from(count).unwrap_or(i64::MAX)),
                        ],
                    )
                    .await
            })
            .map_err(pg_err)
            .map(|_rows_affected| ())
    }

    fn list(&self, repo_id: &str) -> Result<Vec<PackRecord>> {
        let rows = self
            .runtime
            .block_on(async {
                let client = self.client.lock().await;
                client
                    .query(
                        "SELECT pack_id, pack_key, idx_key, object_count
                         FROM git_ents_pack_registry
                         WHERE repo_id = $1 AND pack_id IS NOT NULL",
                        &[&repo_id],
                    )
                    .await
            })
            .map_err(pg_err)?;

        rows.into_iter()
            .map(|row| {
                let pack_id: String = row.try_get(0).map_err(pg_err)?;
                let pack_key: String = row.try_get(1).map_err(pg_err)?;
                let idx_key: String = row.try_get(2).map_err(pg_err)?;
                let object_count: Option<i64> = row.try_get(3).map_err(pg_err)?;
                Ok(PackRecord {
                    id: PackId::new(pack_id),
                    repo_id: repo_id.to_owned(),
                    pack_key,
                    idx_key,
                    object_count: object_count
                        .map(|count| {
                            u64::try_from(count)
                                .map_err(|error| Error::ObjectStore(error.to_string()))
                        })
                        .transpose()?,
                })
            })
            .collect()
    }

    fn delete(&self, repo_id: &str, id: &PackId) -> Result<()> {
        self.runtime
            .block_on(async {
                let client = self.client.lock().await;
                client
                    .execute(
                        "DELETE FROM git_ents_pack_registry WHERE repo_id = $1 AND pack_id = $2",
                        &[&repo_id, &id.as_str()],
                    )
                    .await
            })
            .map_err(pg_err)
            .map(|_rows_affected| ())
    }
}
