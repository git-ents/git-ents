//! [`git_backend::RefStore`] over Postgres — the cloud default backend
//! (`docs/scale-out.adoc`, "RefStore" / WS4).
//!
//! A row per ref, keyed `(repo_id, name)`; `transaction` is one SQL
//! transaction of conditional `UPDATE`/`INSERT`/`DELETE`s, every edit's
//! [`git_backend::Expected`] precondition checked by the statement's own
//! `WHERE` clause and reported via `RETURNING` (see [`ref_store`]).
//! `watch` is `LISTEN`/`NOTIFY`, fired on commit from `transaction`, and
//! remains a wakeup hint only — the `git_ents_effect_queue` table (see
//! [`queue`]) is what an at-least-once consumer actually drains (see
//! [`notify`], and the trait contract on [`git_backend::RefStore::watch`]).
//!
//! This crate also implements two WS5 traits against the same connection,
//! rather than have `odb-tigris`/`odb-tiered` depend on `tokio-postgres`
//! themselves: [`odb_tigris::registry::PackRegistry`] (see
//! [`pack_registry`], over `git_ents_pack_registry`) and
//! [`odb_tiered::small_tier::SmallObjectTier`] (see [`small_tier`], over
//! `git_ents_small_objects`).
//!
//! # Q1: single write-primary
//!
//! This store assumes exactly one writable Postgres primary at a time. Fly
//! managed-Postgres failover semantics must guarantee a fenced single
//! primary or synchronous replication before this backend is deployed
//! against it (`docs/scale-out.adoc`, Q1 / "Non-goals": "no distributed ref
//! consensus"). A split-brain primary — two writers both believing they
//! hold it — is the one unrecoverable failure mode for `RefStore`: two
//! primaries would each serialize CAS locally and correctly, but the two
//! serializations could disagree, which no amount of correct SQL here can
//! detect or repair. Enforcing single-primary is deployment configuration
//! (`fly-replay`, replica topology), not this crate's job — there is no
//! fencing code here, deliberately.

mod corpus;
mod notify;
mod pack_registry;
mod queue;
mod ref_store;
mod small_tier;

pub use queue::{ClaimedEffect, EffectId};

use git_backend::{Error, Result};

/// Migration SQL applied idempotently by [`PostgresRefStore::migrate`]:
/// refs, reflog, pack registry, effect queue, op records (`docs/
/// scale-out.adoc`, WS4's schema list).
const MIGRATION_SQL: &str = include_str!("../migrations/0001_init.sql");

/// The reflog message every transaction's rows carry, mirroring
/// `refstore-files`' fixed `LOG_MESSAGE` — a write through this backend is
/// self-contained, not dependent on caller-supplied metadata.
const LOG_MESSAGE: &str = "git-backend: transaction";

/// [`git_backend::RefStore`] over a Postgres database: one row per ref,
/// scoped to a single `repo_id` (`docs/scale-out.adoc`'s "namespace per
/// repo" rule — no cross-repo query this store issues ever omits the
/// `repo_id` filter).
///
/// Holds one [`tokio_postgres::Client`] behind a [`tokio::sync::Mutex`],
/// per the dependency policy: no connection pool. `transaction` needs
/// exclusive use of the connection for the duration of its SQL transaction
/// (two overlapping `BEGIN`s on one session would corrupt each other), and
/// serializing every other method through the same lock keeps the whole
/// store's concurrency story in one place rather than reasoning about which
/// methods are safe to interleave.
///
/// Owns a dedicated [`tokio::runtime::Runtime`] so [`git_backend::RefStore`]
/// (a sync trait) can drive [`tokio_postgres`]'s async client; every trait
/// method is a `block_on` call.
pub struct PostgresRefStore {
    runtime: tokio::runtime::Runtime,
    client: tokio::sync::Mutex<tokio_postgres::Client>,
    repo_id: String,
    notify: tokio::sync::broadcast::Sender<String>,
}

/// Map a [`tokio_postgres::Error`] onto this crate's shared [`Error`] type.
fn pg_err(error: tokio_postgres::Error) -> Error {
    Error::RefStore(error.to_string())
}

impl PostgresRefStore {
    /// Connect to `conninfo` (a libpq connection string, e.g. `"host=...
    /// user=... dbname=..."`), scope every operation to `repo_id`, apply the
    /// migration (see [`Self::migrate`]), and start listening for this
    /// store's `NOTIFY` channel.
    ///
    /// # Errors
    ///
    /// Returns [`Error::RefStore`] if the connection, migration, or initial
    /// `LISTEN` fails, or if the dedicated Tokio runtime cannot be created.
    pub fn connect(conninfo: &str, repo_id: impl Into<String>) -> Result<Self> {
        let runtime =
            tokio::runtime::Runtime::new().map_err(|error| Error::RefStore(error.to_string()))?;
        let (client, connection) = runtime
            .block_on(tokio_postgres::connect(conninfo, tokio_postgres::NoTls))
            .map_err(pg_err)?;

        let (notify_tx, _receiver) = tokio::sync::broadcast::channel(notify::CHANNEL_CAPACITY);
        runtime.spawn(notify::pump(connection, notify_tx.clone()));

        let store = Self {
            runtime,
            client: tokio::sync::Mutex::new(client),
            repo_id: repo_id.into(),
            notify: notify_tx,
        };
        store.migrate()?;
        store.listen()?;
        Ok(store)
    }

    /// Apply the embedded migration SQL. Every statement is guarded (`CREATE
    /// TABLE IF NOT EXISTS`, `CREATE INDEX IF NOT EXISTS`), so calling this
    /// again against an already-migrated database is a no-op. [`Self::connect`]
    /// already calls this; exposed for callers that want migration as an
    /// explicit, separately-timed step (e.g. a deploy hook run once ahead of
    /// bringing up store instances).
    ///
    /// # Errors
    ///
    /// Returns [`Error::RefStore`] if any migration statement fails.
    pub fn migrate(&self) -> Result<()> {
        self.runtime
            .block_on(async {
                let client = self.client.lock().await;
                client.batch_execute(MIGRATION_SQL).await
            })
            .map_err(pg_err)
    }

    /// Issue this store's `LISTEN`, so `NOTIFY`s fired by any store (this
    /// one or another process's) against the same channel reach this
    /// connection's [`notify::pump`].
    fn listen(&self) -> Result<()> {
        self.runtime
            .block_on(async {
                let client = self.client.lock().await;
                client
                    .batch_execute(&format!("LISTEN {}", notify::CHANNEL))
                    .await
            })
            .map_err(pg_err)
    }
}
