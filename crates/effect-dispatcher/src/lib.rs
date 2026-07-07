//! The WS7 effect dispatcher (`docs/scale-out.adoc`, "WS7 — Effects and
//! Sprites"): one small machine that drains the Postgres effect queue and
//! spawns each claimed effect through an injected
//! [`git_backend::EffectExecutor`] — `exec-local` in a local deployment,
//! `exec-sprites` hosted; the loop cannot tell which, and must not be able
//! to (application code branches on trait capabilities, never on
//! deployment identity).
//!
//! # At-least-once, from the queue table
//!
//! [`git_backend::RefStore::watch`] is a wakeup *hint* only. Every wakeup —
//! a watch hint, a worker slot freeing up, or the periodic poll — runs a
//! full [`Dispatcher::tick`], which requeues stale claims and then claims
//! until the queue is empty or a cap is hit; the poll doubles as the
//! reconnect backstop the watch contract demands, so a dropped
//! LISTEN/NOTIFY notification delays a drain by at most one
//! [`DispatcherConfig::poll_interval`], never loses one. Claims carry a
//! claimant and timestamp; a claim older than
//! [`DispatcherConfig::claim_timeout`] is returned to the queue, so a
//! dispatcher that dies mid-effect redelivers rather than loses — possibly
//! running an effect twice, which is the at-least-once trade: effects are
//! recorded per commit, so a duplicate run re-records the same outcome.
//!
//! # Caps
//!
//! Two knobs bound concurrency: a global cap (cost — every running effect
//! is a machine or a container) and a per-repo cap (fairness — one
//! repository's backlog must not starve the rest). Both are enforced
//! exactly: the drain claims one row per query, recomputing the exclusion
//! set (repositories at their per-repo cap) between claims, so a burst
//! from one repository can never overshoot its cap inside a single batch.
//! One `UPDATE … SKIP LOCKED` round trip per claimed effect is cheap next
//! to what an effect costs to run.

pub mod job;
mod queue;

pub use queue::{EffectQueue, QueuedJob};

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex as StdMutex, PoisonError};
use std::time::Duration;

use git_backend::{EffectExecutor, RefEventStream};

/// The dispatcher's knobs. Both caps are enforced exactly (see the crate
/// docs on claiming one row at a time).
#[derive(Debug, Clone)]
pub struct DispatcherConfig {
    /// The most effects running at once across every repository (cost).
    pub global_cap: usize,
    /// The most effects running at once for one repository (fairness).
    pub per_repo_cap: usize,
    /// How old a claim must be before [`Dispatcher::tick`] returns it to
    /// the queue. Must comfortably exceed the longest legitimate effect
    /// run (the executors' own timeout is 30 minutes), or a slow effect is
    /// redelivered while still running.
    pub claim_timeout: Duration,
    /// The periodic-poll interval: the ceiling on how long a dropped watch
    /// hint can delay a drain.
    pub poll_interval: Duration,
    /// Warm-pool size — always 0 today, and nothing implements a warm pool
    /// beyond this knob. Q3 (`docs/scale-out.adoc`): revisit only if
    /// measured Sprite cold start (image pull included) is *not* ≪ effect
    /// duration; until that measurement exists, a warm pool is cost
    /// without evidence.
    pub warm_pool: usize,
}

impl Default for DispatcherConfig {
    fn default() -> Self {
        Self {
            global_cap: 8,
            per_repo_cap: 2,
            claim_timeout: Duration::from_secs(45 * 60),
            poll_interval: Duration::from_secs(10),
            warm_pool: 0,
        }
    }
}

/// In-flight accounting: how many effects are running globally and per
/// repository. Updated when a worker starts and when it settles; the drain
/// derives its claim budget and exclusion set from it.
#[derive(Debug, Default)]
struct Running {
    global: usize,
    per_repo: HashMap<String, usize>,
}

/// The dispatcher loop: [`Dispatcher::run`] forever in production,
/// [`Dispatcher::tick`] once per wakeup (and directly from tests).
pub struct Dispatcher {
    queue: Arc<dyn EffectQueue>,
    executor: Arc<dyn EffectExecutor>,
    config: DispatcherConfig,
    claimed_by: String,
    running: Arc<StdMutex<Running>>,
    wake_tx: Sender<()>,
    wake_rx: StdMutex<Receiver<()>>,
}

/// Lock `mutex`, recovering the guard from a poisoned lock rather than
/// panicking: losing one wakeup or one count to a poisoned lock is
/// recoverable (the periodic poll re-drains); tearing the dispatcher down
/// is not.
fn lock<T>(mutex: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Dispatcher {
    /// A dispatcher draining `queue` into `executor` under `config`'s caps.
    #[must_use]
    pub fn new(
        queue: Arc<dyn EffectQueue>,
        executor: Arc<dyn EffectExecutor>,
        config: DispatcherConfig,
    ) -> Self {
        let (wake_tx, wake_rx) = std::sync::mpsc::channel();
        Self {
            queue,
            executor,
            config,
            claimed_by: format!("dispatcher-{}", std::process::id()),
            running: Arc::new(StdMutex::new(Running::default())),
            wake_tx,
            wake_rx: StdMutex::new(wake_rx),
        }
    }

    /// Run forever: drain now, then re-drain on every wakeup — a `hints`
    /// event, a worker slot freeing up, or the periodic poll (the
    /// reconnect backstop; see the crate docs).
    pub fn run(&self, hints: RefEventStream) -> ! {
        let forward = self.wake_tx.clone();
        std::thread::spawn(move || {
            while hints.recv().is_some() {
                if forward.send(()).is_err() {
                    break;
                }
            }
        });
        let wake_rx = lock(&self.wake_rx);
        loop {
            self.tick();
            // A hint, a completion, or the poll timeout: which one woke us
            // is deliberately not distinguished — every wakeup re-drains.
            let _wakeup = wake_rx.recv_timeout(self.config.poll_interval);
        }
    }

    /// One full drain: requeue stale claims, then claim-and-start until
    /// the queue is empty or a cap is hit. Idempotent and safe to call on
    /// every wakeup; claims one row per query so both caps are exact (see
    /// the crate docs).
    pub fn tick(&self) {
        if let Err(e) = self.queue.requeue_stale(self.config.claim_timeout) {
            eprintln!("dispatcher: could not requeue stale claims: {e}");
        }
        loop {
            let exclude = {
                let running = lock(&self.running);
                if running.global >= self.config.global_cap {
                    return;
                }
                running
                    .per_repo
                    .iter()
                    .filter(|(_, count)| **count >= self.config.per_repo_cap)
                    .map(|(repo, _)| repo.clone())
                    .collect::<Vec<_>>()
            };
            let claimed = match self.queue.claim(&self.claimed_by, 1, &exclude) {
                Ok(claimed) => claimed,
                Err(e) => {
                    eprintln!("dispatcher: could not claim from the queue: {e}");
                    return;
                }
            };
            let Some(claimed_job) = claimed.into_iter().next() else {
                return;
            };
            self.start(claimed_job);
        }
    }

    /// Decode and spawn one claimed row, handing its wait to a worker
    /// thread that completes the row and frees the slot when the effect
    /// settles.
    ///
    /// Failure semantics, per the at-least-once contract:
    /// - an *undecodable* payload is poison: completed immediately, never
    ///   retried (mirroring how the engine drops a malformed job file);
    /// - a payload that decodes but will not `spawn` (the sandbox is down,
    ///   the launcher errored) stays `claimed`, so the stale-claim timeout
    ///   redelivers it — the work never started, so redelivery is safe;
    /// - a spawned effect is completed once `wait` settles, *whatever* it
    ///   settles to: an executor error after the spawn is a recorded
    ///   outcome, not grounds to run the effect again in-process.
    fn start(&self, claimed_job: QueuedJob) {
        let Some(work) = job::decode(&claimed_job.payload) else {
            eprintln!(
                "dispatcher: dropping malformed payload on queue row {} ({})",
                claimed_job.id, claimed_job.repo
            );
            if let Err(e) = self.queue.complete(claimed_job.id) {
                eprintln!(
                    "dispatcher: could not complete poison row {}: {e}",
                    claimed_job.id
                );
            }
            return;
        };
        let handle = match self.executor.spawn(&work.effect, work.inputs) {
            Ok(handle) => handle,
            Err(e) => {
                eprintln!(
                    "dispatcher: could not spawn {} for {} (left claimed for redelivery): {e}",
                    work.effect.name, claimed_job.repo
                );
                return;
            }
        };

        {
            let mut running = lock(&self.running);
            running.global = running.global.saturating_add(1);
            let count = running
                .per_repo
                .entry(claimed_job.repo.clone())
                .or_insert(0);
            *count = count.saturating_add(1);
        }

        let queue = Arc::clone(&self.queue);
        let executor = Arc::clone(&self.executor);
        let running = Arc::clone(&self.running);
        let wake = self.wake_tx.clone();
        let effect_name = work.effect.name;
        std::thread::spawn(move || {
            match executor.wait(&handle) {
                Ok(status) => eprintln!(
                    "dispatcher: {effect_name} settled for {}: {status:?}",
                    claimed_job.repo
                ),
                Err(e) => eprintln!(
                    "dispatcher: could not observe {effect_name} for {}: {e}",
                    claimed_job.repo
                ),
            }
            if let Err(e) = queue.complete(claimed_job.id) {
                eprintln!(
                    "dispatcher: could not complete queue row {}: {e}",
                    claimed_job.id
                );
            }
            {
                let mut running = lock(&running);
                running.global = running.global.saturating_sub(1);
                if let Some(count) = running.per_repo.get_mut(&claimed_job.repo) {
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        running.per_repo.remove(&claimed_job.repo);
                    }
                }
            }
            // A slot freed: wake the loop so remaining queue rows are
            // claimed now, not on the next poll.
            let _woken = wake.send(());
        });
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "unit test")]

    use std::collections::{BTreeMap, HashSet};
    use std::sync::Condvar;
    use std::time::Instant;

    use git_backend::{EffectDef, EffectHandle, EffectStatus, MaterializedInputs};

    use super::*;

    /// Poll `condition` for up to five seconds — worker settlement runs on
    /// its own threads, so assertions on it are eventual.
    fn eventually(condition: impl Fn() -> bool) -> bool {
        let deadline = Instant::now().checked_add(Duration::from_secs(5)).unwrap();
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        condition()
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum State {
        Enqueued,
        Claimed,
        Done,
    }

    #[derive(Debug, Clone)]
    struct Row {
        id: i64,
        repo: String,
        payload: String,
        state: State,
        claimed_at: Option<Instant>,
        claimed_by: Option<String>,
    }

    /// In-memory [`EffectQueue`] with the table's exact state machine.
    struct FakeQueue {
        rows: StdMutex<Vec<Row>>,
    }

    impl FakeQueue {
        fn new(rows: Vec<Row>) -> Self {
            Self {
                rows: StdMutex::new(rows),
            }
        }

        fn states(&self) -> Vec<State> {
            lock(&self.rows).iter().map(|row| row.state).collect()
        }

        fn all_done(&self) -> bool {
            self.states().iter().all(|state| *state == State::Done)
        }
    }

    impl EffectQueue for FakeQueue {
        fn claim(
            &self,
            claimed_by: &str,
            limit: usize,
            exclude_repos: &[String],
        ) -> git_backend::Result<Vec<QueuedJob>> {
            let mut rows = lock(&self.rows);
            let mut out = Vec::new();
            for row in rows.iter_mut() {
                if out.len() >= limit {
                    break;
                }
                if row.state == State::Enqueued && !exclude_repos.contains(&row.repo) {
                    row.state = State::Claimed;
                    row.claimed_at = Some(Instant::now());
                    row.claimed_by = Some(claimed_by.to_owned());
                    out.push(QueuedJob {
                        id: row.id,
                        repo: row.repo.clone(),
                        payload: row.payload.clone(),
                    });
                }
            }
            Ok(out)
        }

        fn complete(&self, id: i64) -> git_backend::Result<()> {
            for row in lock(&self.rows).iter_mut() {
                if row.id == id {
                    row.state = State::Done;
                }
            }
            Ok(())
        }

        fn requeue_stale(&self, older_than: Duration) -> git_backend::Result<u64> {
            let mut requeued = 0u64;
            for row in lock(&self.rows).iter_mut() {
                let stale = row.state == State::Claimed
                    && row.claimed_at.is_none_or(|at| at.elapsed() > older_than);
                if stale {
                    row.state = State::Enqueued;
                    row.claimed_at = None;
                    row.claimed_by = None;
                    requeued = requeued.saturating_add(1);
                }
            }
            Ok(requeued)
        }
    }

    /// [`EffectExecutor`] whose spawns are recorded and whose completions
    /// the test releases one by one.
    struct FakeExecutor {
        started: StdMutex<Vec<String>>,
        released: StdMutex<HashSet<String>>,
        settle: Condvar,
    }

    impl FakeExecutor {
        fn new() -> Self {
            Self {
                started: StdMutex::new(Vec::new()),
                released: StdMutex::new(HashSet::new()),
                settle: Condvar::new(),
            }
        }

        fn started(&self) -> Vec<String> {
            lock(&self.started).clone()
        }

        fn release(&self, name: &str) {
            lock(&self.released).insert(name.to_owned());
            self.settle.notify_all();
        }
    }

    impl EffectExecutor for FakeExecutor {
        fn spawn(
            &self,
            effect: &EffectDef,
            _inputs: MaterializedInputs,
        ) -> git_backend::Result<EffectHandle> {
            lock(&self.started).push(effect.name.clone());
            Ok(EffectHandle {
                id: effect.name.clone(),
            })
        }

        fn wait(&self, handle: &EffectHandle) -> git_backend::Result<EffectStatus> {
            let deadline = Duration::from_secs(5);
            let mut released = lock(&self.released);
            while !released.contains(&handle.id) {
                let (guard, timeout) = self
                    .settle
                    .wait_timeout(released, deadline)
                    .unwrap_or_else(PoisonError::into_inner);
                released = guard;
                if timeout.timed_out() {
                    return Err(git_backend::Error::Effect(format!(
                        "test effect {} was never released",
                        handle.id
                    )));
                }
            }
            Ok(EffectStatus::Pass)
        }
    }

    fn payload(name: &str) -> String {
        job::encode(&job::Job {
            effect: EffectDef {
                name: name.to_owned(),
                command: Some("true".to_owned()),
                image: None,
            },
            inputs: MaterializedInputs {
                tree: gix_hash::ObjectId::from_hex(b"cccccccccccccccccccccccccccccccccccccccc")
                    .unwrap(),
                toolchain_paths: BTreeMap::new(),
                cache: None,
            },
        })
    }

    fn row(id: i64, repo: &str, name: &str) -> Row {
        Row {
            id,
            repo: repo.to_owned(),
            payload: payload(name),
            state: State::Enqueued,
            claimed_at: None,
            claimed_by: None,
        }
    }

    fn dispatcher(
        rows: Vec<Row>,
        config: DispatcherConfig,
    ) -> (Dispatcher, Arc<FakeQueue>, Arc<FakeExecutor>) {
        let queue = Arc::new(FakeQueue::new(rows));
        let executor = Arc::new(FakeExecutor::new());
        let dispatcher = Dispatcher::new(
            Arc::clone(&queue) as Arc<dyn EffectQueue>,
            Arc::clone(&executor) as Arc<dyn EffectExecutor>,
            config,
        );
        (dispatcher, queue, executor)
    }

    #[test]
    fn a_tick_claims_spawns_and_completes() {
        let (dispatcher, queue, executor) = dispatcher(
            vec![row(1, "repo-a", "fmt"), row(2, "repo-a", "test")],
            DispatcherConfig::default(),
        );
        dispatcher.tick();
        assert_eq!(
            executor.started(),
            vec!["fmt".to_owned(), "test".to_owned()]
        );
        assert_eq!(queue.states(), vec![State::Claimed, State::Claimed]);
        {
            let rows = lock(&queue.rows);
            assert!(
                rows.iter()
                    .all(|row| row.claimed_by.as_deref() == Some(dispatcher.claimed_by.as_str()))
            );
        }

        executor.release("fmt");
        executor.release("test");
        assert!(eventually(|| queue.all_done()));
    }

    #[test]
    fn a_stale_claim_is_requeued_and_redelivered() {
        let mut stale = row(1, "repo-a", "fmt");
        stale.state = State::Claimed;
        stale.claimed_at = Instant::now().checked_sub(Duration::from_secs(600));
        stale.claimed_by = Some("dispatcher-that-died".to_owned());
        let mut fresh = row(2, "repo-b", "test");
        fresh.state = State::Claimed;
        fresh.claimed_at = Some(Instant::now());
        fresh.claimed_by = Some("dispatcher-still-alive".to_owned());

        let config = DispatcherConfig {
            claim_timeout: Duration::from_secs(60),
            ..DispatcherConfig::default()
        };
        let (dispatcher, queue, executor) = dispatcher(vec![stale, fresh], config);
        dispatcher.tick();

        // The stale claim came back and ran; the fresh claim was left with
        // its (living) claimant, not double-delivered.
        assert_eq!(executor.started(), vec!["fmt".to_owned()]);
        executor.release("fmt");
        assert!(eventually(|| queue.states().first() == Some(&State::Done)));
        assert_eq!(queue.states().get(1), Some(&State::Claimed));
    }

    #[test]
    fn the_global_cap_bounds_concurrency() {
        let rows = (1..=5)
            .map(|n| row(n, "repo-a", &format!("effect-{n}")))
            .collect();
        let config = DispatcherConfig {
            global_cap: 2,
            per_repo_cap: 10,
            ..DispatcherConfig::default()
        };
        let (dispatcher, queue, executor) = dispatcher(rows, config);

        dispatcher.tick();
        assert_eq!(executor.started().len(), 2);
        // Re-ticking while saturated claims nothing more.
        dispatcher.tick();
        assert_eq!(executor.started().len(), 2);

        // A freed slot admits exactly one more on the next drain.
        executor.release("effect-1");
        assert!(eventually(|| lock(&dispatcher.running).global == 1));
        dispatcher.tick();
        assert_eq!(executor.started().len(), 3);

        for n in 2..=5 {
            executor.release(&format!("effect-{n}"));
            assert!(eventually(
                || lock(&dispatcher.running).global < dispatcher.config.global_cap
            ));
            dispatcher.tick();
        }
        assert!(eventually(|| queue.all_done()));
        assert_eq!(executor.started().len(), 5);
    }

    #[test]
    fn the_per_repo_cap_keeps_a_backlogged_repo_from_starving_others() {
        // repo-a's three jobs are older (lower ids) than repo-b's one; with
        // a per-repo cap of 1, repo-b must still run immediately.
        let rows = vec![
            row(1, "repo-a", "a-1"),
            row(2, "repo-a", "a-2"),
            row(3, "repo-a", "a-3"),
            row(4, "repo-b", "b-1"),
        ];
        let config = DispatcherConfig {
            global_cap: 8,
            per_repo_cap: 1,
            ..DispatcherConfig::default()
        };
        let (dispatcher, queue, executor) = dispatcher(rows, config);

        dispatcher.tick();
        assert_eq!(executor.started(), vec!["a-1".to_owned(), "b-1".to_owned()]);

        // repo-a proceeds FIFO as its slot frees; repo-b's completion
        // doesn't admit more repo-a work beyond its cap.
        executor.release("a-1");
        assert!(eventually(|| {
            lock(&dispatcher.running).per_repo.get("repo-a").copied() != Some(1)
        }));
        dispatcher.tick();
        assert_eq!(
            executor.started(),
            vec!["a-1".to_owned(), "b-1".to_owned(), "a-2".to_owned()]
        );

        executor.release("a-2");
        executor.release("b-1");
        assert!(eventually(|| lock(&dispatcher.running).per_repo.is_empty()));
        dispatcher.tick();
        executor.release("a-3");
        assert!(eventually(|| queue.all_done()));
    }

    #[test]
    fn a_malformed_payload_is_completed_without_running() {
        let mut poison = row(1, "repo-a", "unused");
        poison.payload = "not a payload".to_owned();
        let (dispatcher, queue, executor) = dispatcher(vec![poison], DispatcherConfig::default());
        dispatcher.tick();
        assert!(executor.started().is_empty());
        assert_eq!(queue.states(), vec![State::Done]);
    }

    #[test]
    fn a_failed_spawn_leaves_the_row_claimed_for_redelivery() {
        /// An executor that refuses every spawn.
        struct DownExecutor;
        impl EffectExecutor for DownExecutor {
            fn spawn(
                &self,
                _effect: &EffectDef,
                _inputs: MaterializedInputs,
            ) -> git_backend::Result<EffectHandle> {
                Err(git_backend::Error::Effect("the sandbox is down".to_owned()))
            }
            fn wait(&self, _handle: &EffectHandle) -> git_backend::Result<EffectStatus> {
                Err(git_backend::Error::Effect("nothing ever spawns".to_owned()))
            }
        }

        let queue = Arc::new(FakeQueue::new(vec![row(1, "repo-a", "fmt")]));
        let dispatcher = Dispatcher::new(
            Arc::clone(&queue) as Arc<dyn EffectQueue>,
            Arc::new(DownExecutor),
            DispatcherConfig::default(),
        );
        dispatcher.tick();
        // Claimed, not done: the work never started, so the stale-claim
        // timeout will redeliver it.
        assert_eq!(queue.states(), vec![State::Claimed]);
    }
}
