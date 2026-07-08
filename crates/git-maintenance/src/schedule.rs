//! Scheduling maintenance (`docs/scale-out.adoc`, WS9 and "Reachability":
//! "Regeneration is scheduled with repack (WS9) and triggered by
//! ref-update volume thresholds").
//!
//! The maintenance effects — GC, cache TTL, consolidation, and
//! reachability regeneration ([`git_reachability::maintenance`], whose
//! scheduling WS6 explicitly deferred here) — are defined as
//! [`EffectDef`]s and enqueued by [`schedule_maintenance`] whenever a
//! repo's accumulated ref-update volume crosses its threshold. The
//! [`Scheduler`] is the piece a server holds: it does the per-repo
//! accumulation so the ingest path only has to report "this push applied
//! N ref edits" (see `git-ents-server`'s native receive-pack endpoint, the
//! wired call site).
//!
//! Like `reachability-maintenance`, these effects run as in-process
//! maintenance code, so their [`EffectDef::command`] is `None`; the queue
//! rows are the schedule, and the runner executes the bodies
//! ([`crate::gc::collect`], [`crate::cache::evict_expired`],
//! [`crate::cache::consolidate`],
//! [`git_reachability::maintenance::regenerate`]) under the per-repo
//! advisory lock ([`crate::lock`]).

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use git_backend::{EffectDef, MaterializedInputs};

use crate::Result;

/// The GC effect's name (mark-and-sweep, [`crate::gc::collect`]).
pub const GC_EFFECT: &str = "maintenance-gc";

/// The cache TTL eviction effect's name ([`crate::cache::evict_expired`]).
pub const CACHE_TTL_EFFECT: &str = "maintenance-cache-ttl";

/// The cache consolidation effect's name ([`crate::cache::consolidate`]).
pub const CONSOLIDATION_EFFECT: &str = "maintenance-cache-consolidation";

/// The static [`EffectDef`] for one in-process maintenance effect —
/// `command`/`image` `None`, mirroring
/// [`git_reachability::maintenance::definition`].
fn definition(name: &str) -> EffectDef {
    EffectDef {
        name: name.to_owned(),
        command: None,
        image: None,
    }
}

/// The ref-update volume thresholds that trigger maintenance. Two knobs
/// because reachability regeneration has its own trigger predicate
/// ([`git_reachability::maintenance::should_regenerate`]) and may
/// reasonably fire less often than repack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Thresholds {
    /// Ref updates before a maintenance run (GC, TTL, consolidation) is
    /// enqueued.
    pub maintenance: u64,
    /// Ref updates before reachability regeneration rides along
    /// ([`git_reachability::maintenance::should_regenerate`]).
    pub reachability: u64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            maintenance: 64,
            reachability: 64,
        }
    }
}

/// Per-repo maintenance-relevant activity since the last scheduled run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    /// Applied ref edits since maintenance was last enqueued for the repo.
    pub ref_updates_since_last: u64,
}

/// Where scheduled maintenance effects land — the effect queue in a
/// Postgres deployment ([`PostgresQueueSink`]), anything else a test or a
/// future local runner supplies.
pub trait MaintenanceSink: Send + Sync {
    /// Enqueue `effects` for `repo_id`, in order.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying queue cannot be written.
    fn enqueue(&self, repo_id: &str, effects: &[EffectDef]) -> Result<()>;
}

/// Enqueue `repo_id`'s maintenance effects into `sink` if `stats` crosses
/// `thresholds.maintenance` — GC, cache TTL, consolidation, plus
/// reachability regeneration when
/// [`git_reachability::maintenance::should_regenerate`] says the volume
/// also warrants that. Returns what was enqueued (empty below threshold).
///
/// # Errors
///
/// Returns an error if the sink fails; nothing is retried here — the
/// caller's accumulated count survives (see [`Scheduler`]) so the next
/// update re-triggers.
pub fn schedule_maintenance(
    repo_id: &str,
    stats: &Stats,
    thresholds: &Thresholds,
    sink: &dyn MaintenanceSink,
) -> Result<Vec<EffectDef>> {
    if stats.ref_updates_since_last < thresholds.maintenance {
        return Ok(Vec::new());
    }
    let mut effects = vec![
        definition(GC_EFFECT),
        definition(CACHE_TTL_EFFECT),
        definition(CONSOLIDATION_EFFECT),
    ];
    if git_reachability::maintenance::should_regenerate(
        stats.ref_updates_since_last,
        thresholds.reachability,
    ) {
        effects.push(git_reachability::maintenance::definition());
    }
    sink.enqueue(repo_id, &effects)?;
    Ok(effects)
}

/// The per-repo accumulator a server holds: ingest reports applied ref
/// edits through [`Scheduler::note_ref_updates`], and once a repo's count
/// crosses the threshold its maintenance effects are enqueued and the
/// count reset. On a sink failure the count is restored, so a transient
/// queue outage delays maintenance rather than losing the trigger.
pub struct Scheduler {
    thresholds: Thresholds,
    sink: Arc<dyn MaintenanceSink>,
    counts: Mutex<HashMap<String, u64>>,
}

impl Scheduler {
    /// A scheduler enqueuing into `sink` at `thresholds`.
    #[must_use]
    pub fn new(thresholds: Thresholds, sink: Arc<dyn MaintenanceSink>) -> Self {
        Self {
            thresholds,
            sink,
            counts: Mutex::new(HashMap::new()),
        }
    }

    /// Record that a push applied `updates` ref edits to `repo_id`,
    /// enqueuing the repo's maintenance effects if that crosses the
    /// threshold. Returns what was enqueued (usually nothing).
    ///
    /// # Errors
    ///
    /// Returns an error if the sink fails — the accumulated count is
    /// restored first, so the trigger is delayed, not lost.
    pub fn note_ref_updates(&self, repo_id: &str, updates: u64) -> Result<Vec<EffectDef>> {
        let due = {
            let mut counts = lock(&self.counts);
            let count = counts.entry(repo_id.to_owned()).or_insert(0);
            *count = count.saturating_add(updates);
            if *count >= self.thresholds.maintenance {
                let accumulated = *count;
                *count = 0;
                Some(accumulated)
            } else {
                None
            }
        };
        let Some(accumulated) = due else {
            return Ok(Vec::new());
        };
        let stats = Stats {
            ref_updates_since_last: accumulated,
        };
        match schedule_maintenance(repo_id, &stats, &self.thresholds, &*self.sink) {
            Ok(effects) => Ok(effects),
            Err(error) => {
                let mut counts = lock(&self.counts);
                let count = counts.entry(repo_id.to_owned()).or_insert(0);
                *count = count.saturating_add(accumulated);
                Err(error)
            }
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// [`MaintenanceSink`] over the Postgres effect queue: each effect is
/// encoded with [`effect_dispatcher::job::encode`] — the payload shape the
/// WS7 dispatcher drains — with a null tree, since maintenance effects run
/// against repository state, not a materialized input tree.
///
/// Connects per enqueue call: enqueues happen once per threshold crossing,
/// not per push, so a short-lived connection is the simple correct choice
/// over holding one open on the ingest path.
pub struct PostgresQueueSink {
    conninfo: String,
}

impl PostgresQueueSink {
    /// A sink enqueuing into the queue at `conninfo` (a libpq connection
    /// string).
    #[must_use]
    pub fn new(conninfo: impl Into<String>) -> Self {
        Self {
            conninfo: conninfo.into(),
        }
    }
}

impl MaintenanceSink for PostgresQueueSink {
    fn enqueue(&self, repo_id: &str, effects: &[EffectDef]) -> Result<()> {
        let store = refstore_postgres::PostgresRefStore::connect(&self.conninfo, repo_id)?;
        for effect in effects {
            let payload = effect_dispatcher::job::encode(&effect_dispatcher::job::Job {
                effect: effect.clone(),
                inputs: MaterializedInputs {
                    tree: gix_hash::ObjectId::null(gix_hash::Kind::Sha1),
                    toolchain_paths: BTreeMap::new(),
                    cache: None,
                },
            });
            store.enqueue_effect(&payload)?;
        }
        Ok(())
    }
}
