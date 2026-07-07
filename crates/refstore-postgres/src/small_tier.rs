//! [`odb_tiered::small_tier::SmallObjectTier`] for [`PostgresRefStore`]:
//! rows in `git_ents_small_objects` (see `migrations/0001_init.sql`),
//! staged then promoted in one `UPDATE` — that single statement is this
//! tier's entire promotion transaction (`docs/scale-out.adoc`, "ObjectStore"
//! / WS5: staged objects invisible until promoted, same contract as
//! [`git_backend::ObjectStore`] itself).
//!
//! Kept in this crate rather than `odb-tiered` itself, for the same reason
//! as [`crate::pack_registry`]: `odb-tiered` depends only on the
//! [`odb_tiered::small_tier::SmallObjectTier`] trait, never on
//! `tokio-postgres`.

use git_backend::{Error, Object, Result};
use gix_hash::ObjectId;
use gix_object::Kind;
use odb_tiered::small_tier::{SmallObjectTier, SmallStageId};

use crate::{PostgresRefStore, pg_err};

/// Render a [`Kind`] as the fixed string stored in `git_ents_small_objects.kind`.
fn kind_to_str(kind: Kind) -> &'static str {
    match kind {
        Kind::Blob => "blob",
        Kind::Tree => "tree",
        Kind::Commit => "commit",
        Kind::Tag => "tag",
    }
}

/// The inverse of [`kind_to_str`].
fn kind_from_str(s: &str) -> Result<Kind> {
    match s {
        "blob" => Ok(Kind::Blob),
        "tree" => Ok(Kind::Tree),
        "commit" => Ok(Kind::Commit),
        "tag" => Ok(Kind::Tag),
        other => Err(Error::ObjectStore(format!(
            "corrupt git_ents_small_objects row: unknown kind {other:?}"
        ))),
    }
}

impl SmallObjectTier for PostgresRefStore {
    fn read(&self, repo_id: &str, id: ObjectId) -> Result<Option<Object>> {
        let hex = id.to_hex().to_string();
        let row = self
            .runtime
            .block_on(async {
                let client = self.client.lock().await;
                client
                    .query_opt(
                        "SELECT kind, bytes FROM git_ents_small_objects
                         WHERE repo_id = $1 AND oid = $2 AND promoted",
                        &[&repo_id, &hex],
                    )
                    .await
            })
            .map_err(pg_err)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let kind: String = row.try_get(0).map_err(pg_err)?;
        let data: Vec<u8> = row.try_get(1).map_err(pg_err)?;
        Ok(Some(Object {
            kind: kind_from_str(&kind)?,
            data,
        }))
    }

    fn contains(&self, repo_id: &str, id: ObjectId) -> Result<bool> {
        let hex = id.to_hex().to_string();
        let row = self
            .runtime
            .block_on(async {
                let client = self.client.lock().await;
                client
                    .query_opt(
                        "SELECT 1 FROM git_ents_small_objects
                         WHERE repo_id = $1 AND oid = $2 AND promoted",
                        &[&repo_id, &hex],
                    )
                    .await
            })
            .map_err(pg_err)?;
        Ok(row.is_some())
    }

    fn stage(&self, repo_id: &str, objects: Vec<(ObjectId, Object)>) -> Result<SmallStageId> {
        let stage_id = SmallStageId::new(uuid::Uuid::new_v4().to_string());
        self.runtime
            .block_on(async {
                let mut client = self.client.lock().await;
                let tx = client.transaction().await?;
                for (oid, object) in &objects {
                    let hex = oid.to_hex().to_string();
                    // Objects are content-addressed: if `(repo_id, oid)`
                    // already has a row — promoted already, or staged by a
                    // still-in-flight batch — its bytes can only be the
                    // same bytes, so leaving it untouched is correct rather
                    // than reattaching it to this new `stage_id` (which
                    // would risk this batch's `promote` racing whatever
                    // state the existing row is already in).
                    tx.execute(
                        "INSERT INTO git_ents_small_objects
                             (repo_id, oid, kind, bytes, stage_id, promoted)
                         VALUES ($1, $2, $3, $4, $5, FALSE)
                         ON CONFLICT (repo_id, oid) DO NOTHING",
                        &[
                            &repo_id,
                            &hex,
                            &kind_to_str(object.kind),
                            &object.data,
                            &stage_id.as_str(),
                        ],
                    )
                    .await?;
                }
                tx.commit().await
            })
            .map_err(pg_err)?;
        Ok(stage_id)
    }

    fn promote(&self, id: SmallStageId) -> Result<()> {
        self.runtime
            .block_on(async {
                let client = self.client.lock().await;
                // The whole promotion, for every object in this batch, is
                // this one statement — there is no distinct "commit" step
                // to get half-applied.
                client
                    .execute(
                        "UPDATE git_ents_small_objects
                         SET promoted = TRUE, stage_id = NULL
                         WHERE stage_id = $1",
                        &[&id.as_str()],
                    )
                    .await
            })
            .map_err(pg_err)
            .map(|_rows_affected| ())
    }
}
