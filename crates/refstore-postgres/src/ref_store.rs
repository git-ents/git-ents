//! [`git_backend::RefStore`] for [`crate::PostgresRefStore`]: transaction =
//! one SQL transaction of conditional `UPDATE`/`INSERT`/`DELETE`s, per
//! `docs/scale-out.adoc`'s `refstore-postgres` row.

use git_backend::{
    Error, Expected, RefEdit, RefEventStream, RefIter, RefLogEntry, RefLogIter, RefName, RefStore,
    Result, TxOutcome,
};
use gix_hash::ObjectId;
use tokio_postgres::Transaction;

use crate::{LOG_MESSAGE, PostgresRefStore, notify, pg_err};

/// The outcome of applying one [`RefEdit`] inside a transaction: whether its
/// [`Expected`] precondition held and, if so, what actually changed (for the
/// reflog row appended alongside it).
enum EditResult {
    /// The precondition failed; the whole transaction must roll back.
    Mismatch,
    /// The precondition held but the edit was a no-op (e.g. `MustNotExist`
    /// against a ref that already didn't exist) — nothing to log.
    NoChange,
    /// The precondition held and the ref's value changed from `old` to
    /// `new`.
    Changed {
        /// The ref's hex oid before this edit, or `None` if it did not
        /// exist.
        old: Option<String>,
        /// The ref's hex oid after this edit, or `None` if it was deleted.
        new: Option<String>,
    },
}

/// Parse a hex string read back from a `TEXT` oid column.
fn parse_oid(hex: &str) -> Result<ObjectId> {
    ObjectId::from_hex(hex.as_bytes()).map_err(|error| Error::RefStore(error.to_string()))
}

/// Escape `%`, `_`, and `\` in `prefix` for use in a `LIKE ... ESCAPE '\'`
/// pattern, then append the wildcard `%`.
fn like_pattern(prefix: &str) -> String {
    let mut pattern = String::with_capacity(prefix.len().saturating_add(1));
    for ch in prefix.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            pattern.push('\\');
        }
        pattern.push(ch);
    }
    pattern.push('%');
    pattern
}

/// Apply one [`RefEdit`] inside `tx`, returning what happened. Every branch
/// is a single conditional statement whose row count (via `RETURNING`)
/// reports whether the precondition held — no separate lock-then-check step
/// is needed; Postgres's own row-level locking on the `UPDATE`/`DELETE`/
/// unique-conflicting `INSERT` makes each branch atomic on its own.
async fn apply_edit(
    tx: &Transaction<'_>,
    repo_id: &str,
    edit: &RefEdit,
) -> std::result::Result<EditResult, tokio_postgres::Error> {
    let name = edit.name.as_str();
    let new_hex = edit.new.map(|oid| oid.to_hex().to_string());

    match (&edit.expected, &new_hex) {
        (Expected::Any, Some(new)) => {
            let old = tx
                .query_opt(
                    "SELECT oid FROM git_ents_refs WHERE repo_id = $1 AND name = $2",
                    &[&repo_id, &name],
                )
                .await?
                .map(|row| row.try_get::<_, String>(0))
                .transpose()?;
            tx.execute(
                "INSERT INTO git_ents_refs (repo_id, name, oid) VALUES ($1, $2, $3)
                 ON CONFLICT (repo_id, name) DO UPDATE SET oid = EXCLUDED.oid",
                &[&repo_id, &name, new],
            )
            .await?;
            Ok(EditResult::Changed {
                old,
                new: Some(new.clone()),
            })
        }
        (Expected::Any, None) => {
            let old = tx
                .query_opt(
                    "SELECT oid FROM git_ents_refs WHERE repo_id = $1 AND name = $2",
                    &[&repo_id, &name],
                )
                .await?
                .map(|row| row.try_get::<_, String>(0))
                .transpose()?;
            if old.is_none() {
                return Ok(EditResult::NoChange);
            }
            tx.execute(
                "DELETE FROM git_ents_refs WHERE repo_id = $1 AND name = $2",
                &[&repo_id, &name],
            )
            .await?;
            Ok(EditResult::Changed { old, new: None })
        }
        (Expected::MustNotExist, Some(new)) => {
            let row = tx
                .query_opt(
                    "INSERT INTO git_ents_refs (repo_id, name, oid) VALUES ($1, $2, $3)
                     ON CONFLICT (repo_id, name) DO NOTHING RETURNING oid",
                    &[&repo_id, &name, new],
                )
                .await?;
            Ok(match row {
                Some(_) => EditResult::Changed {
                    old: None,
                    new: Some(new.clone()),
                },
                None => EditResult::Mismatch,
            })
        }
        (Expected::MustNotExist, None) => {
            let row = tx
                .query_opt(
                    "SELECT 1 FROM git_ents_refs WHERE repo_id = $1 AND name = $2",
                    &[&repo_id, &name],
                )
                .await?;
            Ok(if row.is_none() {
                EditResult::NoChange
            } else {
                EditResult::Mismatch
            })
        }
        (Expected::MustExistAndMatch(expected), Some(new)) => {
            let expected_hex = expected.to_hex().to_string();
            let row = tx
                .query_opt(
                    "UPDATE git_ents_refs SET oid = $3 WHERE repo_id = $1 AND name = $2 AND oid = $4
                     RETURNING oid",
                    &[&repo_id, &name, new, &expected_hex],
                )
                .await?;
            Ok(match row {
                Some(_) => EditResult::Changed {
                    old: Some(expected_hex),
                    new: Some(new.clone()),
                },
                None => EditResult::Mismatch,
            })
        }
        (Expected::MustExistAndMatch(expected), None) => {
            let expected_hex = expected.to_hex().to_string();
            let row = tx
                .query_opt(
                    "DELETE FROM git_ents_refs WHERE repo_id = $1 AND name = $2 AND oid = $3
                     RETURNING oid",
                    &[&repo_id, &name, &expected_hex],
                )
                .await?;
            Ok(match row {
                Some(_) => EditResult::Changed {
                    old: Some(expected_hex),
                    new: None,
                },
                None => EditResult::Mismatch,
            })
        }
    }
}

impl RefStore for PostgresRefStore {
    fn get(&self, name: &RefName) -> Result<Option<ObjectId>> {
        self.runtime.block_on(async {
            let client = self.client.lock().await;
            let row = client
                .query_opt(
                    "SELECT oid FROM git_ents_refs WHERE repo_id = $1 AND name = $2",
                    &[&self.repo_id, &name.as_str()],
                )
                .await
                .map_err(pg_err)?;
            match row {
                Some(row) => {
                    let hex: String = row.try_get(0).map_err(pg_err)?;
                    parse_oid(&hex).map(Some)
                }
                None => Ok(None),
            }
        })
    }

    fn iter_prefix(&self, prefix: &RefName) -> Result<RefIter> {
        let pattern = like_pattern(prefix.as_str());
        let rows = self
            .runtime
            .block_on(async {
                let client = self.client.lock().await;
                client
                    .query(
                        "SELECT name, oid FROM git_ents_refs
                         WHERE repo_id = $1 AND name LIKE $2 ESCAPE '\\'
                         ORDER BY name",
                        &[&self.repo_id, &pattern],
                    )
                    .await
            })
            .map_err(pg_err)?;

        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            out.push((|| {
                let name: String = row.try_get(0).map_err(pg_err)?;
                let hex: String = row.try_get(1).map_err(pg_err)?;
                Ok((RefName::new(name), parse_oid(&hex)?))
            })());
        }
        Ok(RefIter::new(out.into_iter()))
    }

    fn transaction(&self, edits: &[RefEdit]) -> Result<TxOutcome> {
        self.runtime.block_on(async {
            let mut client = self.client.lock().await;
            let tx = client.transaction().await.map_err(pg_err)?;

            let mut changed_names = Vec::new();
            for edit in edits {
                match apply_edit(&tx, &self.repo_id, edit).await.map_err(pg_err)? {
                    EditResult::Mismatch => {
                        tx.rollback().await.map_err(pg_err)?;
                        return Ok(TxOutcome::Rejected {
                            name: edit.name.clone(),
                        });
                    }
                    EditResult::NoChange => {}
                    EditResult::Changed { old, new } => {
                        tx.execute(
                            "INSERT INTO git_ents_reflog
                                (repo_id, name, old_oid, new_oid, message)
                             VALUES ($1, $2, $3, $4, $5)",
                            &[&self.repo_id, &edit.name.as_str(), &old, &new, &LOG_MESSAGE],
                        )
                        .await
                        .map_err(pg_err)?;
                        changed_names.push(edit.name.as_str().to_owned());
                    }
                }
            }

            if !changed_names.is_empty() {
                let payload = notify::encode(&self.repo_id, &changed_names);
                tx.execute("SELECT pg_notify($1, $2)", &[&notify::CHANNEL, &payload])
                    .await
                    .map_err(pg_err)?;
            }
            tx.commit().await.map_err(pg_err)?;
            Ok(TxOutcome::Applied)
        })
    }

    fn watch(&self, prefix: &RefName) -> Result<RefEventStream> {
        Ok(bridge_watch(
            &self.runtime,
            self.notify.subscribe(),
            self.repo_id.clone(),
            prefix.as_str().to_owned(),
        ))
    }

    fn log(&self, name: &RefName) -> Result<RefLogIter> {
        let rows = self
            .runtime
            .block_on(async {
                let client = self.client.lock().await;
                client
                    .query(
                        "SELECT old_oid, new_oid, message,
                                extract(epoch from recorded_at)::bigint
                         FROM git_ents_reflog
                         WHERE repo_id = $1 AND name = $2
                         ORDER BY id DESC",
                        &[&self.repo_id, &name.as_str()],
                    )
                    .await
            })
            .map_err(pg_err)?;

        let mut entries = Vec::with_capacity(rows.len());
        for row in rows {
            entries.push((|| {
                let old: Option<String> = row.try_get(0).map_err(pg_err)?;
                let new: Option<String> = row.try_get(1).map_err(pg_err)?;
                let message: String = row.try_get(2).map_err(pg_err)?;
                let seconds: i64 = row.try_get(3).map_err(pg_err)?;
                Ok(RefLogEntry {
                    old: old.as_deref().map(parse_oid).transpose()?,
                    new: new.as_deref().map(parse_oid).transpose()?,
                    message,
                    seconds: u64::try_from(seconds).unwrap_or(0),
                })
            })());
        }
        Ok(RefLogIter::new(entries.into_iter()))
    }
}

/// Bridge a [`tokio::sync::broadcast::Receiver`] of raw `NOTIFY` payloads
/// into the blocking [`RefEventStream`] the trait exposes: a background task
/// filters every payload by `repo_id`/`prefix` and forwards a match as a
/// [`git_backend::RefEvent`]. A lagged receiver (the subscriber fell behind
/// and missed payloads) still emits a hint — per `watch`'s contract, a
/// consumer never trusts hint precision, only that draining its own state
/// is due.
fn bridge_watch(
    runtime: &tokio::runtime::Runtime,
    mut receiver: tokio::sync::broadcast::Receiver<String>,
    repo_id: String,
    prefix: String,
) -> RefEventStream {
    let (sender, events) = std::sync::mpsc::channel();
    runtime.spawn(async move {
        loop {
            match receiver.recv().await {
                Ok(payload) => {
                    if !notify::matches(&payload, &repo_id, &prefix) {
                        continue;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
            if sender.send(git_backend::RefEvent).is_err() {
                break;
            }
        }
    });
    RefEventStream::new(events)
}
