//! Asynchronous effect running: a `post-receive` hook that *queues* a push and
//! a server-owned worker that runs the configured effects against it in a
//! Fly.io [Sprite].
//!
//! Effects run *after* the refs are in and off the push connection. The hook
//! ([`post_receive`]) does almost nothing: it reads the pushed ref updates git
//! feeds it on stdin and drops a job file into the shared queue directory, so
//! the push returns immediately. The long-running server drains that queue
//! from a dedicated worker ([`worker`]); for each job it loads the effect set
//! from [`crate::definition::EFFECTS_NS`] and runs every effect in a Sprite —
//! a persistent, hardware-isolated sandbox. One Sprite is kept per repository
//! so its filesystem (and any build cache an effect leaves behind) survives
//! between pushes; the pushed tree is synced into it before the effects run.
//! Results are recorded as run refs (and surfaced on the Checks tab), and
//! logged to the server's own output rather than relayed to the pusher.
//!
//! The Sprite is driven through the `sprite` CLI. The CLI authenticates from a
//! config file rather than the environment, so the worker first hands it the
//! `SPRITES_TOKEN` the server passes down via `sprite auth setup`; only then
//! does an organization become configured.
//!
//! [Sprite]: https://sprites.dev

use std::collections::HashMap;
use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex as StdMutex, PoisonError};
use std::time::{Duration, Instant};

use gix_hash::ObjectId;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use tokio::sync::Mutex;

use crate::cache;
use crate::definition::{self, Effect};
use crate::docker;
use crate::local;
use crate::results::{self, RunOutcome, Status};

/// Where the pushed tree is unpacked inside the Sprite.
const WORKDIR: &str = "/work";

/// Where resolved toolchains are extracted inside the Sprite, one directory
/// per tree hash (`{TOOLCHAINS_DIR}/<hash>`) — unlike [`WORKDIR`], never
/// cleared: the Sprite's persistent filesystem is the extract-once cache.
const TOOLCHAINS_DIR: &str = "/toolchains";

/// A currently-running effect's growing asciicast v2 recording, keyed by the
/// repository, the commit being checked, and the effect's name.
pub type LiveKey = (PathBuf, ObjectId, String);

/// Live buffers for every effect currently running, shared between the
/// worker thread appending to an effect's output as it arrives and the web
/// layer polling it for a live view. A buffer exists only while its effect is
/// running — [`live_start`] adds it, [`live_finish`] removes it once the
/// result is recorded — so a lookup miss unambiguously means "not running"
/// rather than "running with no output yet". Asciicast is the definitive log
/// format end to end: the same string a live poll reads is, unmodified,
/// what [`run_one`] hands back as the effect's recorded `recording`.
pub type LiveRegistry = Arc<StdMutex<HashMap<LiveKey, Arc<StdMutex<String>>>>>;

/// A fresh, empty [`LiveRegistry`] — one per server process, held on the
/// server's shared state.
#[must_use]
pub fn new_live_registry() -> LiveRegistry {
    Arc::new(StdMutex::new(HashMap::new()))
}

/// The text accumulated so far for a running effect's live buffer, or `None`
/// when no effect is running under `key` (finished, or never started).
#[must_use]
pub fn live_snapshot(registry: &LiveRegistry, key: &LiveKey) -> Option<String> {
    let buffer = lock(registry).get(key).cloned()?;
    Some(lock(&buffer).clone())
}

/// Register a fresh live buffer for `key`, returning the handle [`run_one`]
/// appends to as the effect's output arrives.
fn live_start(registry: &LiveRegistry, key: LiveKey) -> Arc<StdMutex<String>> {
    let buffer = Arc::new(StdMutex::new(String::new()));
    lock(registry).insert(key, Arc::clone(&buffer));
    buffer
}

/// Remove `key`'s live buffer once its effect has settled — recorded results
/// are read from the run ref from then on, not the live registry.
fn live_finish(registry: &LiveRegistry, key: &LiveKey) {
    lock(registry).remove(key);
}

/// Lock a [`StdMutex`], recovering the guard from a poisoned lock rather than
/// panicking: a live buffer is best-effort output for a browser to look at,
/// not something worth tearing the process down over if a prior panic
/// poisoned it.
fn lock<T>(mutex: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The environment variable through which the server hands the hook the queue
/// directory; the worker is given the same path directly.
pub const QUEUE_ENV: &str = "GIT_ENTS_CHECKS_QUEUE";

/// How often the worker scans the queue directory for new jobs.
const POLL: Duration = Duration::from_secs(2);

/// Queue the push git is reporting for asynchronous effect running, returning
/// `Ok(())` once the jobs are enqueued. The ref updates are read from the
/// stdin git populates for a `post-receive` hook (`<old> <new> <ref>` lines).
///
/// The hook does no effect work itself: it writes one job file per updated
/// branch into the shared queue directory ([`QUEUE_ENV`]) and returns, so the
/// push is never blocked on a Sprite. The server's [`worker`] picks the jobs
/// up.
///
/// ## Requirements
///
/// @relation(checks.post-receive, nonfunctional.push-latency)
pub fn post_receive() -> Result<(), String> {
    let repo = std::env::current_dir().map_err(|e| format!("cannot resolve repository: {e}"))?;

    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .map_err(|e| format!("could not read ref updates: {e}"))?;
    let updates = parse_updates(&input);
    if updates.is_empty() {
        return Ok(());
    }

    // An empty effect set leaves nothing to queue.
    let runnable =
        definition::load_all(&repo).map_err(|e| format!("could not read effects: {e}"))?;
    if runnable.is_empty() {
        return Ok(());
    }

    let Some(queue) = std::env::var_os(QUEUE_ENV).map(PathBuf::from) else {
        eprintln!("effects: {QUEUE_ENV} is not set; skipping asynchronous effects");
        return Ok(());
    };

    for update in updates {
        enqueue(&queue, &repo, &update)?;
        // Record the run as `queued` straight away so it shows up on the Checks
        // tab the moment the push lands, before the worker picks it up; a
        // recording hiccup is reported but never fails the hook.
        let queued = statuses(&runnable, Status::Queued);
        if let Err(e) = results::record(&repo, update.new, &queued) {
            eprintln!(
                "effects: could not record queued run for {}: {e}",
                update.new
            );
        }
        println!(
            "effects: queued {} effect(s) on {}",
            runnable.len(),
            update.ref_name
        );
    }
    Ok(())
}

/// Every effect's [`RunOutcome`] set to one shared `status` — the queued/running
/// snapshot a run starts from before per-effect results land.
fn statuses(effects: &[Effect], status: Status) -> Vec<RunOutcome> {
    effects
        .iter()
        .map(|effect| RunOutcome {
            name: effect.name.clone(),
            status,
            duration_secs: None,
            recording: None,
            exit_code: None,
        })
        .collect()
}

/// Run the worker that drains the queue directory, running and recording the
/// effects for each queued push. Runs for the life of the server; the blocking
/// Sprite work is offloaded so it never stalls the async runtime.
///
/// Jobs are processed per repository: each tick, every repository with pending
/// jobs that is not already being worked gets its own blocking task that drains
/// its jobs in order. Because an effect can run up to [`CHECK_TIMEOUT`], serving
/// all repositories from one queue scan would let a single slow repository stall
/// every other repository's effects; isolating them by repository keeps a slow
/// repository's backlog from blocking the rest. Jobs for *one* repository stay
/// serialized so concurrent runs never collide in its single Sprite.
///
/// ## Requirements
///
/// @relation(checks.worker, nonfunctional.concurrency, checks.sandbox)
pub async fn worker(queue: PathBuf, live: LiveRegistry, kind: BackendKind) {
    if let Err(e) = std::fs::create_dir_all(&queue) {
        eprintln!("effects: could not create queue directory {queue:?}: {e}");
        return;
    }
    let inflight: Arc<Mutex<HashSet<PathBuf>>> = Arc::new(Mutex::new(HashSet::new()));
    let mut tick = tokio::time::interval(POLL);
    loop {
        tick.tick().await;
        let mut guard = inflight.lock().await;
        for (repo, jobs) in pending_jobs(&queue) {
            // Skip a repository already draining; its task will pick up any jobs
            // that arrived since on its next scan.
            if !guard.insert(repo.clone()) {
                continue;
            }
            let inflight = Arc::clone(&inflight);
            let live = live.clone();
            let handle = tokio::task::spawn_blocking(move || drain_repo(&jobs, &live, kind));
            tokio::spawn(async move {
                let _done = handle.await;
                inflight.lock().await.remove(&repo);
            });
        }
    }
}

/// Which sandbox a job's effects run in.
///
/// [`Sprite`](BackendKind::Sprite) is the hosted backend, driven through the
/// `sprite` CLI. [`Docker`](BackendKind::Docker) is the local default (`git
/// effect run`, and `git ents serve`'s own worker): a throwaway container per
/// effect, with toolchains materialized on the host and bind-mounted in — see
/// [`crate::local`] and [`crate::docker`]. [`Host`](BackendKind::Host) is
/// `--unsandboxed`: the command runs directly on the machine running the
/// worker, no isolation at all.
///
/// ## Requirements
///
/// @relation(checks.sandbox)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    /// The hosted Fly.io Sprite backend.
    Sprite,
    /// The local Docker backend.
    Docker,
    /// Host-direct execution (`--unsandboxed`), no sandbox at all.
    Host,
}

/// The backend `git ents serve`/`git-ents-server` fall back to when not told
/// otherwise: [`BackendKind::Sprite`] when `SPRITES_TOKEN` is set in the
/// environment — the hosted deployment's own signal that a Sprite is
/// configured (see [`ensure_auth`]) — [`BackendKind::Docker`] otherwise. This
/// is exactly the Deployment table's split: hosted mode always carries
/// `SPRITES_TOKEN`, local `git ents serve` never does.
///
/// ## Requirements
///
/// @relation(checks.sandbox)
#[must_use]
pub fn default_backend() -> BackendKind {
    backend_for_token(std::env::var("SPRITES_TOKEN").ok().as_deref())
}

/// [`default_backend`]'s pure decision, taking `SPRITES_TOKEN`'s value
/// directly rather than reading the environment — the part worth unit
/// testing without mutating process-global state.
fn backend_for_token(sprites_token: Option<&str>) -> BackendKind {
    if sprites_token.is_some() {
        BackendKind::Sprite
    } else {
        BackendKind::Docker
    }
}

/// The pending jobs in the queue directory grouped by repository, so each
/// repository can be drained independently. A malformed job file is dropped here
/// rather than grouped — a poison job is never retried.
fn pending_jobs(queue: &Path) -> HashMap<PathBuf, Vec<(PathBuf, Job)>> {
    let mut groups: HashMap<PathBuf, Vec<(PathBuf, Job)>> = HashMap::new();
    let Ok(entries) = std::fs::read_dir(queue) else {
        return groups;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "job") {
            continue;
        }
        match read_job(&path) {
            Some(job) => groups
                .entry(job.repo.clone())
                .or_default()
                .push((path, job)),
            None => {
                let _removed = std::fs::remove_file(&path);
            }
        }
    }
    groups
}

/// Drain one repository's queued jobs in order, deleting each job file after it
/// is handled (whether it ran cleanly or failed) so it is never retried.
fn drain_repo(jobs: &[(PathBuf, Job)], live: &LiveRegistry, kind: BackendKind) {
    for (path, job) in jobs {
        if let Err(e) = process_job(job, live, kind) {
            eprintln!("effects: {e}");
        }
        let _removed = std::fs::remove_file(path);
    }
}

/// Run one queued job's effects in `kind`'s backend, discarding the outcomes
/// (already recorded — see [`run_all`]) since nothing else needs them here.
///
/// ## Requirements
///
/// @relation(checks.worker)
fn process_job(job: &Job, live: &LiveRegistry, kind: BackendKind) -> Result<(), String> {
    run_all(&job.repo, job.new, &job.ref_name, kind, live)?;
    Ok(())
}

/// Run every configured effect in `repo` against `at` outside the queue —
/// `git effect run`'s local execution path. Identical toolchain
/// materialization and sandbox path to a push-triggered run (see
/// [`run_all`]); only the queue is skipped, exactly as the porcelain
/// promises. `at` is a full hex commit id, already resolved by the caller.
///
/// ## Requirements
///
/// @relation(checks.worker, cli.account-checks)
pub fn run_effect_at(
    repo: &Path,
    at: &str,
    kind: BackendKind,
    live: &LiveRegistry,
) -> Result<Vec<RunOutcome>, String> {
    let oid = ObjectId::from_hex(at.trim().as_bytes())
        .map_err(|e| format!("{at:?} is not a valid commit id: {e}"))?;
    run_all(repo, oid, "<local run>", kind, live)
}

/// Run every effect for `new` in `repo`'s given backend, advancing the
/// recorded run as it goes: `running` while the sandbox is prepared, then
/// each effect flipped to its result as it finishes. Effects settle in the
/// dependency order `definition::order` fixed at write time: an effect whose
/// dependency did not pass is recorded `skipped` without touching the
/// sandbox, and a composite (no command) derives its status from its
/// dependencies alone. An infra failure (an unreachable sandbox, a tree that
/// will not sync, an effect set that fails re-validation) finalizes the run
/// as `error` rather than leaving it stuck at `running`, then returns `Err`.
/// Returns the settled outcomes on success — even one that includes a
/// failing effect, which is a recorded result, not an error.
///
/// ## Requirements
///
/// @relation(checks.worker, checks.sandbox)
fn run_all(
    repo: &Path,
    new: ObjectId,
    ref_name: &str,
    kind: BackendKind,
    live: &LiveRegistry,
) -> Result<Vec<RunOutcome>, String> {
    let runnable =
        definition::load_all(repo).map_err(|e| format!("could not read effects: {e}"))?;
    if runnable.is_empty() {
        return Ok(Vec::new());
    }

    let mut outcomes = statuses(&runnable, Status::Running);
    // Re-validate defensively: the CLI rejects an invalid graph before it is
    // pushed, but a hand-crafted push could still land one. Indices into
    // `runnable`/`outcomes` rather than borrows, so outcomes stay mutable.
    let ordered: Vec<usize> = match definition::order(&runnable) {
        Ok(ordered) => ordered
            .iter()
            .filter_map(|effect| runnable.iter().position(|c| c.name == effect.name))
            .collect(),
        Err(e) => {
            finalize_error(repo, new, &mut outcomes);
            return Err(format!("invalid effect set: {e}"));
        }
    };

    let backend = match Backend::new(kind, repo) {
        Ok(backend) => backend,
        Err(e) => {
            finalize_error(repo, new, &mut outcomes);
            return Err(e);
        }
    };
    if let Err(e) = backend.ensure() {
        finalize_error(repo, new, &mut outcomes);
        return Err(e);
    }

    eprintln!(
        "effects: running {} effect(s) on {}",
        runnable.len(),
        ref_name
    );
    advance(repo, new, &outcomes);
    if let Err(e) = backend.sync_tree(repo, new) {
        finalize_error(repo, new, &mut outcomes);
        return Err(e);
    }

    let toolchain_dirs = match backend.resolve_toolchains(repo, &runnable) {
        Ok(dirs) => dirs,
        Err(e) => {
            finalize_error(repo, new, &mut outcomes);
            return Err(e);
        }
    };

    let mut cache_names: Vec<&str> = runnable
        .iter()
        .filter_map(|effect| effect.cache.as_deref())
        .collect();
    cache_names.sort_unstable();
    cache_names.dedup();
    for name in cache_names {
        if let Err(e) = backend.restore_cache(repo, name) {
            finalize_error(repo, new, &mut outcomes);
            return Err(e);
        }
    }

    for index in ordered {
        let Some(effect) = runnable.get(index) else {
            continue;
        };
        // Topological order guarantees every dependency settled already.
        let deps: Vec<Status> = effect
            .depends
            .iter()
            .filter_map(|dep| {
                outcomes
                    .iter()
                    .find(|outcome| outcome.name == *dep)
                    .map(|outcome| outcome.status)
            })
            .collect();
        let all_pass = deps.iter().all(|status| *status == Status::Pass);
        match &effect.command {
            Some(command) if all_pass => {
                let command = activate(command, &effect.toolchains, &toolchain_dirs);
                let cache_dir = effect
                    .cache
                    .as_deref()
                    .map(|name| backend.cache_dir_for(name));
                let command = with_cache_env(&command, cache_dir.as_deref());
                let key: LiveKey = (repo.to_path_buf(), new, effect.name.clone());
                let buffer = live_start(live, key.clone());
                let result = backend.run_one(&effect.name, &command, &buffer);
                live_finish(live, &key);
                if let Some(name) = &effect.cache
                    && let Err(e) = backend.snapshot_cache(repo, name)
                {
                    eprintln!("effects: could not snapshot cache {name}: {e}");
                }
                if let Some(outcome) = outcomes.get_mut(index) {
                    outcome.status = result.status;
                    outcome.duration_secs = Some(result.duration_secs);
                    outcome.recording = Some(result.recording);
                    outcome.exit_code = result.exit_code;
                }
            }
            Some(_) => {
                eprintln!("effects: SKIP {} (a dependency did not pass)", effect.name);
                if let Some(outcome) = outcomes.get_mut(index) {
                    outcome.status = Status::Skipped;
                }
            }
            None => {
                let status = derive_composite(&deps);
                eprintln!(
                    "effects: {} {} (composite)",
                    status.to_string().to_uppercase(),
                    effect.name
                );
                if let Some(outcome) = outcomes.get_mut(index) {
                    outcome.status = status;
                }
            }
        }
        advance(repo, new, &outcomes);
    }
    Ok(outcomes)
}

/// One ready-to-use sandbox backend: the Sprite's name, or a fresh
/// [`local::Sandbox`] materialized on the host for the Docker or host-direct
/// backends. Constructing it is the one place a backend-specific setup
/// failure (no `docker` on `PATH`, no scratch directory) surfaces before any
/// sandbox work starts.
///
/// ## Requirements
///
/// @relation(checks.sandbox)
enum Backend {
    Sprite(String),
    Docker(local::Sandbox),
    Host(local::Sandbox),
}

impl Backend {
    fn new(kind: BackendKind, repo: &Path) -> Result<Self, String> {
        match kind {
            BackendKind::Sprite => Ok(Backend::Sprite(sprite_name(repo))),
            BackendKind::Docker => {
                docker::ensure_docker()?;
                Ok(Backend::Docker(local::Sandbox::new()?))
            }
            BackendKind::Host => Ok(Backend::Host(local::Sandbox::new()?)),
        }
    }

    /// Sprite-only setup (auth, create-if-absent); the local backends need
    /// none, since [`Backend::new`] already prepared their sandbox.
    fn ensure(&self) -> Result<(), String> {
        match self {
            Backend::Sprite(name) => ensure_auth().and_then(|()| ensure_sprite(name)),
            Backend::Docker(_) | Backend::Host(_) => Ok(()),
        }
    }

    fn sync_tree(&self, repo: &Path, new: ObjectId) -> Result<(), String> {
        match self {
            Backend::Sprite(name) => sync_tree(repo, name, new),
            Backend::Docker(sandbox) | Backend::Host(sandbox) => {
                local::sync_tree(repo, sandbox, new)
            }
        }
    }

    /// A `name -> PATH entry` map: an in-Sprite/in-container path for the
    /// Sprite and Docker backends, the real host path for host-direct
    /// execution, since it runs with no container to bind-mount into.
    fn resolve_toolchains(
        &self,
        repo: &Path,
        runnable: &[Effect],
    ) -> Result<HashMap<String, String>, String> {
        match self {
            Backend::Sprite(name) => resolve_toolchains(repo, name, runnable),
            Backend::Docker(sandbox) => {
                let names = local::resolve_toolchains(repo, sandbox, runnable)?;
                Ok(names
                    .into_iter()
                    .map(|name| {
                        let dir = format!("{}/{name}/bin", docker::TOOLCHAINS_DIR);
                        (name, dir)
                    })
                    .collect())
            }
            Backend::Host(sandbox) => {
                let names = local::resolve_toolchains(repo, sandbox, runnable)?;
                Ok(local::host_toolchain_dirs(sandbox, &names))
            }
        }
    }

    fn restore_cache(&self, repo: &Path, name: &str) -> Result<(), String> {
        match self {
            Backend::Sprite(sprite) => cache::restore(repo, sprite, name),
            Backend::Docker(sandbox) | Backend::Host(sandbox) => {
                cache::restore_local(repo, &sandbox.cache_dir(name), name)
            }
        }
    }

    fn snapshot_cache(&self, repo: &Path, name: &str) -> Result<(), String> {
        match self {
            Backend::Sprite(sprite) => cache::snapshot(repo, sprite, name),
            Backend::Docker(sandbox) | Backend::Host(sandbox) => {
                cache::snapshot_local(repo, &sandbox.cache_dir(name), name)
            }
        }
    }

    fn cache_dir_for(&self, name: &str) -> String {
        match self {
            Backend::Sprite(_) => cache::cache_dir(name),
            Backend::Docker(_) => format!("{}/{name}", docker::CACHE_DIR),
            Backend::Host(sandbox) => sandbox.cache_dir(name).display().to_string(),
        }
    }

    fn run_one(&self, name: &str, command: &str, live: &Arc<StdMutex<String>>) -> RunResult {
        match self {
            Backend::Sprite(sprite) => run_one(sprite, name, command, live),
            Backend::Docker(sandbox) => run_one_docker(sandbox, name, command, live),
            Backend::Host(sandbox) => run_one_host(sandbox, name, command, live),
        }
    }
}

/// A composite effect's status, derived from its dependencies' settled
/// statuses: `pass` when everything passed, `fail` when anything failed or
/// errored, `skipped` when nothing failed but something was skipped.
///
/// ## Requirements
///
/// @relation(checks.worker)
fn derive_composite(deps: &[Status]) -> Status {
    if deps.iter().all(|status| *status == Status::Pass) {
        Status::Pass
    } else if deps
        .iter()
        .any(|status| matches!(status, Status::Fail | Status::Error))
    {
        Status::Fail
    } else {
        Status::Skipped
    }
}

/// Advance the recorded run for `new` to `outcomes`; a recording hiccup is
/// logged but never derails the worker.
fn advance(repo: &Path, new: ObjectId, outcomes: &[RunOutcome]) {
    if let Err(e) = results::update_run(repo, new, outcomes) {
        eprintln!("effects: could not record run for {new}: {e}");
    }
}

/// Mark every effect in `outcomes` `error` and record it — the terminal state
/// for a run the worker could not carry out.
///
/// ## Requirements
///
/// @relation(checks.worker)
fn finalize_error(repo: &Path, new: ObjectId, outcomes: &mut [RunOutcome]) {
    for outcome in outcomes.iter_mut() {
        outcome.status = Status::Error;
    }
    advance(repo, new, outcomes);
}

/// One queued push: the repository to check, the new tip to check, and the ref
/// it updated (carried only for logging).
struct Job {
    repo: PathBuf,
    new: ObjectId,
    ref_name: String,
}

/// Write a job for `update` into `queue` as a three-line file (`repo`, new oid,
/// ref). The file is written under a `.tmp` name and renamed into place so the
/// worker never observes a half-written job.
///
/// ## Requirements
///
/// @relation(checks.post-receive)
fn enqueue(queue: &Path, repo: &Path, update: &Update) -> Result<(), String> {
    std::fs::create_dir_all(queue)
        .map_err(|e| format!("could not create queue directory {queue:?}: {e}"))?;
    let stem = job_stem();
    let tmp = queue.join(format!("{stem}.tmp"));
    let final_path = queue.join(format!("{stem}.job"));
    let body = format!("{}\n{}\n{}\n", repo.display(), update.new, update.ref_name);
    std::fs::write(&tmp, body).map_err(|e| format!("could not write job: {e}"))?;
    std::fs::rename(&tmp, &final_path).map_err(|e| format!("could not enqueue job: {e}"))?;
    Ok(())
}

/// A unique job file stem so concurrent pushes never collide on a queue file
/// name.
fn job_stem() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Parse a queued job file (`repo`, new oid, ref, one per line), or `None` when
/// it is malformed.
fn read_job(path: &Path) -> Option<Job> {
    let contents = std::fs::read_to_string(path).ok()?;
    let mut lines = contents.lines();
    let repo = PathBuf::from(lines.next()?);
    let new = ObjectId::from_hex(lines.next()?.as_bytes()).ok()?;
    let ref_name = lines.next()?.to_owned();
    Some(Job {
        repo,
        new,
        ref_name,
    })
}

/// One ref git reported as updated by the push.
struct Update<'a> {
    new: ObjectId,
    ref_name: &'a str,
}

/// Refname prefixes an effect can never be triggered by, no matter how broad
/// a trigger pattern gets (even `refs/meta/*` or `refs/*`): a push under
/// [`crate::results::RESULTS_NS`] is an effect's own recorded outcome, and a
/// future `refs/meta/index/*` namespace is server-maintained derived state —
/// letting either enqueue effects would let a result (or an index update)
/// trigger the effect that produced it, recursing forever. Checked ahead of,
/// and independently from, the broader `refs/meta/` exclusion in
/// [`parse_updates`], so the invariant holds even once a per-effect `trigger`
/// pattern exists and could otherwise opt into these namespaces.
const NEVER_TRIGGERS: &[&str] = &["refs/meta/results/", "refs/meta/index/"];

/// Whether `ref_name` may ever enqueue effects. Always `false` for
/// [`NEVER_TRIGGERS`]' namespaces, regardless of any trigger pattern an
/// effect declares.
fn triggers_effects(ref_name: &str) -> bool {
    !NEVER_TRIGGERS
        .iter()
        .any(|prefix| ref_name.starts_with(prefix))
}

/// Parse git's `<old-oid> <new-oid> <ref>` stdin into the updates worth
/// checking: branch updates with a real new tip. Deletions (a zero new oid),
/// the `refs/meta/*` control refs (auth, the effect set itself), and anything
/// under [`NEVER_TRIGGERS`] are skipped — the effects gate ordinary content,
/// not the trust plumbing or an effect's own recorded results.
fn parse_updates(input: &str) -> Vec<Update<'_>> {
    input
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let _old = fields.next()?;
            let new = fields.next()?;
            let ref_name = fields.next()?;
            let new = ObjectId::from_hex(new.as_bytes()).ok()?;
            if new.is_null() || ref_name.starts_with("refs/meta/") || !triggers_effects(ref_name) {
                None
            } else {
                Some(Update { new, ref_name })
            }
        })
        .collect()
}

/// A Sprite name derived from the repository directory, kept to the
/// `[a-z0-9-]` a Sprite name allows so the same repo reuses the same sandbox.
///
/// Shared with the web layer's debug-session broker, which targets the same
/// persistent per-repo Sprite an effect run used.
///
/// ## Requirements
///
/// @relation(checks.sandbox)
#[must_use]
pub fn sprite_name(repo: &Path) -> String {
    let stem = repo
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| "repo".into());
    let sanitized: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = sanitized.trim_matches('-');
    format!(
        "checks-{}",
        if trimmed.is_empty() { "repo" } else { trimmed }
    )
}

/// Configure the `sprite` CLI from the `SPRITES_TOKEN` the server passes down.
/// The CLI persists its credentials to a config file rather than reading the
/// token per call, so without this it reports "no organizations configured"
/// even with the token in the environment. `auth setup` is idempotent, so it is
/// run on every push to keep the steady state self-healing.
///
/// ## Requirements
///
/// @relation(checks.sandbox, compat.sprite)
pub fn ensure_auth() -> Result<(), String> {
    let token = std::env::var("SPRITES_TOKEN")
        .ok()
        .ok_or("SPRITES_TOKEN is not set in the hook environment")?;
    let output = Command::new("sprite")
        .args(["auth", "setup", "--token", &token])
        .output()
        .map_err(|e| format!("could not run the sprite CLI (is it installed?): {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "sprite auth setup failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// Create the repository's Sprite if it does not already exist. `sprite create`
/// fails when the Sprite is already there, which is the steady state once the
/// first push has run, so its failure is tolerated and surfaces only later if
/// the Sprite turns out to be unreachable.
///
/// ## Requirements
///
/// @relation(checks.sandbox, compat.sprite)
pub fn ensure_sprite(sprite: &str) -> Result<(), String> {
    let _existing = Command::new("sprite")
        .args(["create", "--skip-console", sprite])
        .output()
        .map_err(|e| format!("could not run the sprite CLI (is it installed?): {e}"))?;
    Ok(())
}

/// Stream the pushed tree at `new` into the Sprite's [`WORKDIR`] via
/// [`unpack_script`]. `git archive` emits the tree as a tar that the Sprite
/// unpacks over stdin.
///
/// ## Requirements
///
/// @relation(checks.sandbox, compat.sprite, compat.git)
fn sync_tree(repo: &Path, sprite: &str, new: ObjectId) -> Result<(), String> {
    let archive = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["archive", "--format=tar", &new.to_string()])
        .output()
        .map_err(|e| format!("could not run git archive: {e}"))?;
    if !archive.status.success() {
        return Err(format!("git archive failed for {new}"));
    }

    let script = unpack_script();
    let mut child = Command::new("sprite")
        .args(["exec", "-s", sprite, "--", "sh", "-c", &script])
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run the sprite CLI: {e}"))?;
    child
        .stdin
        .take()
        .ok_or("sprite exec did not accept stdin")?
        .write_all(&archive.stdout)
        .map_err(|e| format!("could not stream the tree into the sprite: {e}"))?;
    let status = child
        .wait()
        .map_err(|e| format!("sprite exec did not complete: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("could not unpack the tree in the sprite".to_owned())
    }
}

/// The in-sprite script that replaces [`WORKDIR`]'s contents with the tar
/// streamed over stdin, leaving the rest of the persistent filesystem (build
/// caches and the like) intact.
///
/// It first kills any process still working under [`WORKDIR`]: a worker
/// killed mid-run (a deploy, a restart) leaves its in-sprite build processes
/// alive, since `sprite exec` only tethers the local CLI process — and an
/// orphaned build still writing under [`WORKDIR`] races the wipe, failing
/// `rm -rf` with "Directory not empty". The final `rm -rf && mkdir && tar`
/// chain is what the exec's exit status reflects, as before.
fn unpack_script() -> String {
    format!(
        "for cwd in /proc/[0-9]*/cwd; do\n\
           case \"$(readlink \"$cwd\" 2>/dev/null)\" in\n\
             {WORKDIR}|{WORKDIR}/*) kill -9 \"$(basename \"${{cwd%/cwd}}\")\" 2>/dev/null || true ;;\n\
           esac\n\
         done\n\
         rm -rf {WORKDIR} && mkdir -p {WORKDIR} && tar -x -C {WORKDIR}"
    )
}

/// Resolve and extract every distinct toolchain named across `runnable`,
/// returning each name's extracted `bin` directory inside the Sprite. A
/// failed resolution (the named ref does not exist) is the one place
/// `definition::order` could not have caught it, since `refs/meta/toolchains/*`
/// is a different namespace than the effect set itself.
///
/// ## Requirements
///
/// @relation(checks.toolchains, checks.sandbox)
fn resolve_toolchains(
    repo: &Path,
    sprite: &str,
    runnable: &[Effect],
) -> Result<HashMap<String, String>, String> {
    let mut names: Vec<&str> = runnable
        .iter()
        .flat_map(|effect| effect.toolchains.iter().map(String::as_str))
        .collect();
    names.sort_unstable();
    names.dedup();

    let mut dirs = HashMap::new();
    for name in names {
        let toolchain = git_toolchain::resolve(repo, name)
            .map_err(|e| format!("could not resolve toolchain {name}: {e}"))?;
        let dir = match &toolchain.bin {
            git_toolchain::Bin::Embedded(tree) => {
                let tree = tree.oid();
                sync_toolchain(repo, sprite, tree)?;
                format!("{TOOLCHAINS_DIR}/{tree}")
            }
            git_toolchain::Bin::Downloaded(components) => {
                let key = components_key(components);
                sync_downloaded_toolchain(sprite, &key, components)?;
                // Unlike an embedded toolchain's tree (already flattened to
                // put executables at its own top level), each component
                // extracts per its recorded layout — its own `bin/` top level
                // (rustup) or straight into a `bin` dest (a flat archive) —
                // landing executables at `<key>/bin` either way, so `PATH`
                // points one level deeper.
                format!("{TOOLCHAINS_DIR}/{key}/bin")
            }
        };
        dirs.insert(name.to_owned(), dir);
    }
    Ok(dirs)
}

/// A stable, filesystem-safe cache key for a [`git_toolchain::Bin::Downloaded`]
/// toolchain: each component's sha256 plus its recorded layout
/// (`strip`/`dest` — the same bytes extracted differently are a different
/// toolchain on disk), joined in extraction order — there is no tree oid to
/// key the extraction cache by, since nothing is written to the object
/// database for a downloaded toolchain's `bin`.
fn components_key(components: &[git_toolchain::Component]) -> String {
    components
        .iter()
        .map(|component| {
            format!(
                "{}.{}.{}",
                component.sha256, component.strip, component.dest
            )
        })
        .collect::<Vec<_>>()
        .join("-")
}

/// Prefix `command` with a `PATH` export activating `toolchains`' extracted
/// `bin` directories, declared order first (so the first-listed toolchain's
/// `bin` wins on a name collision); an effect with no toolchains is returned
/// unchanged.
///
/// ## Requirements
///
/// @relation(checks.sandbox)
fn activate(command: &str, toolchains: &[String], dirs: &HashMap<String, String>) -> String {
    if toolchains.is_empty() {
        return command.to_owned();
    }
    let path = toolchains
        .iter()
        .filter_map(|name| dirs.get(name))
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(":");
    format!("export PATH={path}:$PATH; {command}")
}

/// Prefix `command` with an `EFFECT_CACHE_DIR` export pointing at
/// `cache_dir` (the cache's restored directory in whichever backend is
/// running — see [`Backend::cache_dir_for`]), so the command can point a
/// tool (`sccache`, ...) at it; an effect with no cache is returned
/// unchanged.
///
/// ## Requirements
///
/// @relation(checks.cache)
fn with_cache_env(command: &str, cache_dir: Option<&str>) -> String {
    match cache_dir {
        Some(dir) => format!("export EFFECT_CACHE_DIR={dir}; {command}"),
        None => command.to_owned(),
    }
}

/// Extract the toolchain tree `tree` into the Sprite at
/// `{TOOLCHAINS_DIR}/<tree>`, once — a directory already there from an
/// earlier push is left alone rather than re-extracted, since the Sprite's
/// persistent filesystem is the cache. Checked before running `git archive`
/// so an already-cached toolchain never streams its (potentially large)
/// contents through a pipe the Sprite has no reason to read.
///
/// Extraction happens into a sibling `.tmp` directory and only lands at `dir`
/// via a final `mv`, so a transient failure partway through (e.g. a truncated
/// stream) never leaves `dir` existing-but-incomplete: the next push's cache
/// check sees no directory at all and retries, instead of trusting a half
/// extraction forever.
///
/// ## Requirements
///
/// @relation(checks.sandbox)
fn sync_toolchain(repo: &Path, sprite: &str, tree: ObjectId) -> Result<(), String> {
    let dir = format!("{TOOLCHAINS_DIR}/{tree}");
    let cached = Command::new("sprite")
        .args([
            "exec",
            "-s",
            sprite,
            "--",
            "sh",
            "-c",
            &format!("[ -d {dir} ]"),
        ])
        .status()
        .map_err(|e| format!("could not run the sprite CLI: {e}"))?;
    if cached.success() {
        return Ok(());
    }

    let archive = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["archive", "--format=tar", &tree.to_string()])
        .output()
        .map_err(|e| format!("could not run git archive: {e}"))?;
    if !archive.status.success() {
        return Err(format!("git archive failed for toolchain {tree}"));
    }

    let tmp = format!("{dir}.tmp");
    let script = format!(
        "rm -rf {tmp} && mkdir -p {tmp} && tar -x -C {tmp} && rm -rf {dir} && mv {tmp} {dir}"
    );
    let mut child = Command::new("sprite")
        .args(["exec", "-s", sprite, "--", "sh", "-c", &script])
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run the sprite CLI: {e}"))?;
    child
        .stdin
        .take()
        .ok_or("sprite exec did not accept stdin")?
        .write_all(&archive.stdout)
        .map_err(|e| format!("could not stream the toolchain into the sprite: {e}"))?;
    let status = child
        .wait()
        .map_err(|e| format!("sprite exec did not complete: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("could not extract toolchain {tree} in the sprite"))
    }
}

/// Fetch, sha256-verify, and extract a [`git_toolchain::Bin::Downloaded`]
/// toolchain's components into the Sprite at `{TOOLCHAINS_DIR}/<key>`, once —
/// same cache-once discipline as [`sync_toolchain`], keyed by
/// [`components_key`] since there is no tree oid to key by. Verification and
/// extraction both happen inside the Sprite via `curl`/`sha256sum`/`tar`,
/// mirroring `git_toolchain::export`'s local equivalent: downloading through
/// the server first and streaming the bytes in would defeat the point of not
/// storing them.
///
/// ## Requirements
///
/// @relation(checks.sandbox)
fn sync_downloaded_toolchain(
    sprite: &str,
    key: &str,
    components: &[git_toolchain::Component],
) -> Result<(), String> {
    let dir = format!("{TOOLCHAINS_DIR}/{key}");
    let cached = Command::new("sprite")
        .args([
            "exec",
            "-s",
            sprite,
            "--",
            "sh",
            "-c",
            &format!("[ -d {dir} ]"),
        ])
        .status()
        .map_err(|e| format!("could not run the sprite CLI: {e}"))?;
    if cached.success() {
        return Ok(());
    }

    let script = downloaded_script(&dir, components);
    let status = Command::new("sprite")
        .args(["exec", "-s", sprite, "--", "sh", "-c", &script])
        .status()
        .map_err(|e| format!("could not run the sprite CLI: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "could not fetch and extract downloaded toolchain {key} in the sprite"
        ))
    }
}

/// The `sh` script fetching, verifying, and extracting `components` into
/// `dir` — pure, so the exact extraction semantics the Sprite runs are unit
/// tested against `git_toolchain::export`'s local equivalent (the
/// local/hosted parity anchor). Each component lands in `dir`/its `dest`,
/// stripped of its leading `strip` path segments, compression auto-detected
/// by `tar` (rust-lang ships gzip, zig ships xz). Interpolation is safe by
/// construction: `git_toolchain::import_downloaded` refuses a component
/// whose fields could escape the single quotes.
///
/// Every component extracts into a sibling `.tmp` directory first; `dir`
/// itself is only populated by the final `mv`, once every component has
/// fetched, verified, and extracted successfully. A mid-script failure (a
/// flaky `curl`, a hash mismatch) then leaves no directory at `dir` at all,
/// so [`sync_downloaded_toolchain`]'s cache check retries on the next push
/// instead of reusing a partially-extracted toolchain forever.
///
/// ## Requirements
///
/// @relation(checks.sandbox)
fn downloaded_script(dir: &str, components: &[git_toolchain::Component]) -> String {
    let tmp = format!("{dir}.tmp");
    let mut script = format!("rm -rf {tmp} && mkdir -p {tmp}");
    for component in components {
        let dest = if component.dest.is_empty() {
            tmp.clone()
        } else {
            format!("{tmp}/{}", component.dest)
        };
        script.push_str(&format!(
            " && mkdir -p {dest} \
               && curl -fsSL '{url}' -o /tmp/component.archive \
               && [ \"$(sha256sum /tmp/component.archive | cut -d' ' -f1)\" = '{sha256}' ] \
               && tar -x --strip-components={strip} -C {dest} -f /tmp/component.archive \
               && rm -f /tmp/component.archive",
            url = component.url,
            sha256 = component.sha256,
            strip = component.strip,
        ));
    }
    script.push_str(&format!(" && rm -rf {dir} && mv {tmp} {dir}"));
    script
}

/// How long a single effect may run before the worker abandons it. A runaway
/// effect that outlived this — a hung build, a command blocked on input — is
/// killed and recorded `error` rather than wedging the worker (and with it every
/// other repository's effects) on the one blocking-pool thread the queue drains
/// on.
///
/// ## Requirements
///
/// @relation(checks.outcomes)
const CHECK_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// The fixed size an effect's recorded terminal session runs at. Nothing
/// interactive ever attaches to it, so this only shapes the recording, not
/// anyone's actual terminal.
const CHECK_PTY_SIZE: PtySize = PtySize {
    rows: 24,
    cols: 80,
    pixel_width: 0,
    pixel_height: 0,
};

/// A finished effect run: its outcome, wall-clock duration, process exit code
/// (when the command ran to completion), and the full terminal session as an
/// asciicast v2 recording.
struct RunResult {
    status: Status,
    duration_secs: u64,
    recording: String,
    exit_code: Option<i32>,
}

/// Run one effect in the Sprite's [`WORKDIR`], recording its terminal session —
/// a real pty (`sprite exec --tty`), not a pipe, so the recording plays back
/// exactly what a developer running the effect by hand would see — and logging
/// a `PASS`/`FAIL` line. `live` is appended to as output arrives, in the same
/// asciicast v2 format as the final recording, so a browser can poll it for a
/// live view of an effect still in progress; it is what [`finish`] hands back
/// as the recorded `recording`, not a separate representation of the same
/// output. Returns the effect's outcome; an effect that exceeds
/// [`CHECK_TIMEOUT`] or cannot be captured is [`Status::Error`].
///
/// ## Requirements
///
/// @relation(compat.sprite)
fn run_one(sprite: &str, name: &str, command: &str, live: &Arc<StdMutex<String>>) -> RunResult {
    let start = Instant::now();
    lock(live).push_str(&asciicast_header());

    let pair = match native_pty_system().openpty(CHECK_PTY_SIZE) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("effects: ERROR {name} (could not allocate a pty: {e})");
            return finish(Status::Error, start, None, live);
        }
    };
    let mut cmd = CommandBuilder::new("sprite");
    cmd.args([
        "exec", "--tty", "-s", sprite, "--dir", WORKDIR, "--", "sh", "-c", command,
    ]);
    let mut child = match pair.slave.spawn_command(cmd) {
        Ok(child) => child,
        Err(e) => {
            eprintln!("effects: ERROR {name} (could not run: {e})");
            return finish(Status::Error, start, None, live);
        }
    };
    // The child holds the slave now; drop ours so the master sees EOF when the
    // effect process actually exits rather than when this scope happens to end.
    drop(pair.slave);

    let master = pair.master;
    let Ok(reader) = master.try_clone_reader() else {
        eprintln!("effects: ERROR {name} (could not read the pty)");
        let _killed = child.kill();
        return finish(Status::Error, start, None, live);
    };

    let timed_out = drain(reader, start, live);
    drop(master);

    if timed_out {
        eprintln!("effects: ERROR {name} (timed out after {CHECK_TIMEOUT:?})");
        let _killed = child.kill();
        return finish(Status::Error, start, None, live);
    }

    let status = match child.wait() {
        Ok(status) => status,
        Err(e) => {
            eprintln!("effects: ERROR {name} (could not wait on the sprite CLI: {e})");
            return finish(Status::Error, start, None, live);
        }
    };

    let exit_code = Some(i32::try_from(status.exit_code()).unwrap_or(i32::MAX));
    if status.success() {
        eprintln!("effects: PASS {name}");
        finish(Status::Pass, start, exit_code, live)
    } else {
        eprintln!("effects: FAIL {name} ({command})");
        finish(Status::Fail, start, exit_code, live)
    }
}

/// Run one effect in the Docker backend's throwaway `--rm` container, per
/// [`docker::run_args`]. Otherwise identical to [`run_one`]: same timeout,
/// same asciicast recording, same `live` buffer — just a plain pipe instead
/// of a pty, since nothing here needs an interactive terminal, only a
/// captured one.
///
/// ## Requirements
///
/// @relation(checks.sandbox)
fn run_one_docker(
    sandbox: &local::Sandbox,
    name: &str,
    command: &str,
    live: &Arc<StdMutex<String>>,
) -> RunResult {
    let start = Instant::now();
    lock(live).push_str(&asciicast_header());

    let args = docker::run_args(
        &sandbox.work_dir(),
        &sandbox.toolchains_dir(),
        &sandbox.cache_root(),
        command,
    );
    let mut cmd = Command::new("docker");
    cmd.args(&args);
    run_captured(&mut cmd, name, command, start, live)
}

/// Run one effect directly on the host (`--unsandboxed`), in the sandbox's
/// materialized work directory — no container, no isolation. Otherwise
/// identical to [`run_one_docker`].
///
/// ## Requirements
///
/// @relation(checks.sandbox)
fn run_one_host(
    sandbox: &local::Sandbox,
    name: &str,
    command: &str,
    live: &Arc<StdMutex<String>>,
) -> RunResult {
    let start = Instant::now();
    lock(live).push_str(&asciicast_header());

    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(format!("{command} 2>&1"))
        .current_dir(sandbox.work_dir());
    run_captured(&mut cmd, name, command, start, live)
}

/// Spawn `cmd` (already built, stdout not yet configured), capture its
/// combined output into `live` via [`drain`], and assemble the [`RunResult`]
/// — the part [`run_one_docker`] and [`run_one_host`] share.
fn run_captured(
    cmd: &mut Command,
    name: &str,
    command: &str,
    start: Instant,
    live: &Arc<StdMutex<String>>,
) -> RunResult {
    let mut child = match cmd.stdin(Stdio::null()).stdout(Stdio::piped()).spawn() {
        Ok(child) => child,
        Err(e) => {
            eprintln!("effects: ERROR {name} (could not run: {e})");
            return finish(Status::Error, start, None, live);
        }
    };
    let Some(stdout) = child.stdout.take() else {
        eprintln!("effects: ERROR {name} (could not capture output)");
        let _killed = child.kill();
        return finish(Status::Error, start, None, live);
    };

    let timed_out = drain(stdout, start, live);
    if timed_out {
        eprintln!("effects: ERROR {name} (timed out after {CHECK_TIMEOUT:?})");
        let _killed = child.kill();
        return finish(Status::Error, start, None, live);
    }

    let status = match child.wait() {
        Ok(status) => status,
        Err(e) => {
            eprintln!("effects: ERROR {name} (could not wait: {e})");
            return finish(Status::Error, start, None, live);
        }
    };

    let exit_code = status.code();
    if status.success() {
        eprintln!("effects: PASS {name}");
        finish(Status::Pass, start, exit_code, live)
    } else {
        eprintln!("effects: FAIL {name} ({command})");
        finish(Status::Fail, start, exit_code, live)
    }
}

/// Read `reader` until EOF or [`CHECK_TIMEOUT`] elapses since `start`,
/// appending each chunk to `live` as an asciicast v2 output event, exactly
/// the format [`run_one`]'s pty capture already produces. Shared by every
/// backend so the Checks tab's live/final recording looks the same
/// regardless of which one ran: the Sprite backend feeds this a pty's
/// reader, the Docker/host backends a plain child pipe. `reader`'s own
/// (blocking) read runs on a dedicated thread; the caller's thread only
/// waits on a channel, so it can time out the whole run without depending on
/// the read itself returning promptly. Returns whether the timeout (rather
/// than EOF) ended the read.
fn drain(
    mut reader: impl Read + Send + 'static,
    start: Instant,
    live: &Arc<StdMutex<String>>,
) -> bool {
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let Some(chunk) = buf.get(..n) else { break };
                    if tx.send(chunk.to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let deadline = start.checked_add(CHECK_TIMEOUT).unwrap_or(start);
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return true;
        };
        match rx.recv_timeout(remaining) {
            Ok(chunk) => {
                let elapsed = start.elapsed().as_secs_f64();
                let data = String::from_utf8_lossy(&chunk);
                push_event(&mut lock(live), elapsed, &data);
            }
            Err(RecvTimeoutError::Timeout) => return true,
            Err(RecvTimeoutError::Disconnected) => return false,
        }
    }
}

/// Assemble a [`RunResult`] from `live`'s accumulated recording — used on
/// every exit path, including the failure ones, so an effect that errors out
/// still keeps whatever terminal output it produced before that happened.
fn finish(
    status: Status,
    start: Instant,
    exit_code: Option<i32>,
    live: &StdMutex<String>,
) -> RunResult {
    RunResult {
        status,
        duration_secs: start.elapsed().as_secs(),
        recording: lock(live).clone(),
        exit_code,
    }
}

/// The asciicast v2 header line naming the terminal's fixed [`CHECK_PTY_SIZE`]
/// — the first line of every effect recording, live or finished (see
/// <https://docs.asciinema.org/manual/asciicast/v2/>).
fn asciicast_header() -> String {
    format!(
        "{{\"version\": 2, \"width\": {}, \"height\": {}}}\n",
        CHECK_PTY_SIZE.cols, CHECK_PTY_SIZE.rows
    )
}

/// Append one asciicast v2 `[time, "o", data]` output event to `out`, the
/// Checks tab's replay format for a chunk of pty output captured `time`
/// seconds into the run.
fn push_event(out: &mut String, time: f64, data: &str) {
    out.push('[');
    out.push_str(&format!("{time:.6}"));
    out.push_str(", \"o\", ");
    push_json_string(data, out);
    out.push_str("]\n");
}

/// Append `value` to `out` as a quoted JSON string. Hand-rolled rather than
/// taking on a JSON crate for this one call site: escape what JSON requires
/// (`"`, `\`, and the C0 control codes) and pass the rest — already valid
/// UTF-8, since it came from `String::from_utf8_lossy` — straight through.
fn push_json_string(value: &str, out: &mut String) {
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, reason = "unit test")]

    use super::*;

    // @relation(checks.post-receive, role=Verifies)
    #[test]
    fn parse_updates_keeps_content_branches_only() {
        let new = "1111111111111111111111111111111111111111";
        let zero = "0".repeat(40);
        let input = format!(
            "{zero} {new} refs/heads/main\n\
             {new} {zero} refs/heads/old\n\
             {new} {new} refs/meta/effects/fmt\n\
             {new} {new} refs/heads/feature\n",
        );
        let updates = parse_updates(&input);
        let refs: Vec<&str> = updates.iter().map(|u| u.ref_name).collect();
        assert_eq!(refs, vec!["refs/heads/main", "refs/heads/feature"]);
    }

    // @relation(checks.post-receive, role=Verifies)
    #[test]
    fn triggers_effects_hard_excludes_results_and_index_regardless_of_pattern() {
        // A `refs/*`-broad trigger must never fire on a push under
        // `refs/meta/results/*` or `refs/meta/index/*` — the exclusion is
        // independent of how permissive an effect's own trigger pattern is.
        assert!(!triggers_effects("refs/meta/results/fmt/abc123"));
        assert!(!triggers_effects("refs/meta/index/abc123"));
        // Ordinary content refs are unaffected.
        assert!(triggers_effects("refs/heads/main"));
        assert!(triggers_effects("refs/meta/effects/fmt"));
    }

    // @relation(checks.post-receive, role=Verifies)
    #[test]
    fn parse_updates_never_enqueues_a_results_ref_push() {
        let new = "1".repeat(40);
        let old = "0".repeat(40);
        let input = format!(
            "{old} {new} refs/meta/results/fmt/abcdef123456\n{old} {new} refs/heads/main\n"
        );
        let updates = parse_updates(&input);
        let refs: Vec<&str> = updates.iter().map(|u| u.ref_name).collect();
        assert_eq!(refs, vec!["refs/heads/main"]);
    }

    // @relation(checks.sandbox, role=Verifies)
    #[test]
    fn activate_leaves_a_toolchain_free_command_unchanged() {
        let dirs = HashMap::new();
        assert_eq!(activate("cargo test", &[], &dirs), "cargo test");
    }

    // @relation(checks.sandbox, role=Verifies)
    #[test]
    fn activate_prefixes_path_in_declared_order() {
        let mut dirs = HashMap::new();
        dirs.insert("gcc".to_owned(), "/toolchains/aaa".to_owned());
        dirs.insert("cmake".to_owned(), "/toolchains/bbb".to_owned());
        let toolchains = vec!["gcc".to_owned(), "cmake".to_owned()];
        assert_eq!(
            activate("make", &toolchains, &dirs),
            "export PATH=/toolchains/aaa:/toolchains/bbb:$PATH; make"
        );
    }

    // @relation(checks.sandbox, role=Verifies)
    #[test]
    fn activate_skips_a_toolchain_missing_from_dirs() {
        let dirs = HashMap::new();
        let toolchains = vec!["gcc".to_owned()];
        assert_eq!(
            activate("make", &toolchains, &dirs),
            "export PATH=:$PATH; make"
        );
    }

    // @relation(checks.sandbox, role=Verifies)
    #[test]
    fn downloaded_script_extracts_each_component_per_its_layout() {
        let components = vec![
            git_toolchain::Component {
                url: "https://static.rust-lang.org/dist/rustc.tar.gz".to_owned(),
                sha256: "aaa".to_owned(),
                strip: 2,
                dest: String::new(),
            },
            git_toolchain::Component {
                url: "https://example.com/flat.tar.xz".to_owned(),
                sha256: "bbb".to_owned(),
                strip: 1,
                dest: "bin".to_owned(),
            },
        ];
        assert_eq!(
            downloaded_script("/toolchains/key", &components),
            "rm -rf /toolchains/key.tmp \
             && mkdir -p /toolchains/key.tmp \
             && mkdir -p /toolchains/key.tmp \
             && curl -fsSL 'https://static.rust-lang.org/dist/rustc.tar.gz' -o /tmp/component.archive \
             && [ \"$(sha256sum /tmp/component.archive | cut -d' ' -f1)\" = 'aaa' ] \
             && tar -x --strip-components=2 -C /toolchains/key.tmp -f /tmp/component.archive \
             && rm -f /tmp/component.archive \
             && mkdir -p /toolchains/key.tmp/bin \
             && curl -fsSL 'https://example.com/flat.tar.xz' -o /tmp/component.archive \
             && [ \"$(sha256sum /tmp/component.archive | cut -d' ' -f1)\" = 'bbb' ] \
             && tar -x --strip-components=1 -C /toolchains/key.tmp/bin -f /tmp/component.archive \
             && rm -f /tmp/component.archive \
             && rm -rf /toolchains/key \
             && mv /toolchains/key.tmp /toolchains/key"
        );
    }

    // @relation(checks.sandbox, role=Verifies)
    #[test]
    fn unpack_script_kills_stale_processes_before_the_wipe() {
        let script = unpack_script();
        let kill = script.find("kill -9").unwrap();
        let wipe = script.find("rm -rf").unwrap();
        assert!(kill < wipe);
        assert!(script.ends_with("rm -rf /work && mkdir -p /work && tar -x -C /work"));
    }

    // @relation(checks.sandbox, role=Verifies)
    #[test]
    fn components_key_includes_the_layout() {
        let component = git_toolchain::Component {
            url: "https://example.com/a.tar.gz".to_owned(),
            sha256: "aaa".to_owned(),
            strip: 2,
            dest: String::new(),
        };
        let mut flat = component.clone();
        flat.strip = 1;
        flat.dest = "bin".to_owned();
        assert_eq!(components_key(std::slice::from_ref(&component)), "aaa.2.");
        assert_ne!(
            components_key(std::slice::from_ref(&component)),
            components_key(&[flat])
        );
    }

    // @relation(checks.cache, role=Verifies)
    #[test]
    fn with_cache_env_leaves_a_cache_free_command_unchanged() {
        assert_eq!(with_cache_env("cargo build", None), "cargo build");
    }

    // @relation(checks.cache, role=Verifies)
    #[test]
    fn with_cache_env_exports_the_restored_directory() {
        assert_eq!(
            with_cache_env("cargo build", Some("/cache/sccache")),
            "export EFFECT_CACHE_DIR=/cache/sccache; cargo build"
        );
    }

    // @relation(checks.worker, role=Verifies)
    #[test]
    fn composite_status_derives_from_its_dependencies() {
        assert_eq!(
            derive_composite(&[Status::Pass, Status::Pass]),
            Status::Pass
        );
        assert_eq!(
            derive_composite(&[Status::Pass, Status::Fail]),
            Status::Fail
        );
        assert_eq!(
            derive_composite(&[Status::Error, Status::Skipped]),
            Status::Fail
        );
        assert_eq!(
            derive_composite(&[Status::Pass, Status::Skipped]),
            Status::Skipped
        );
        // Vacuously all-pass: a composite with no dependencies never validates,
        // but the derivation itself is total.
        assert_eq!(derive_composite(&[]), Status::Pass);
    }

    // @relation(checks.worker, role=Verifies)
    #[test]
    fn pending_jobs_groups_by_repo_and_drops_malformed() {
        let queue = tempfile::tempdir().unwrap();
        let write = |name: &str, body: &str| {
            std::fs::write(queue.path().join(name), body).unwrap();
        };
        let oid_a = "a".repeat(40);
        let oid_b = "b".repeat(40);
        let oid_c = "c".repeat(40);
        let oid_d = "d".repeat(40);
        write("a.job", &format!("/repos/one\n{oid_a}\nrefs/heads/main\n"));
        write("b.job", &format!("/repos/one\n{oid_b}\nrefs/heads/dev\n"));
        write("c.job", &format!("/repos/two\n{oid_c}\nrefs/heads/main\n"));
        write("d.job", "garbage\n");
        write(
            "ignored.tmp",
            &format!("/repos/one\n{oid_d}\nrefs/heads/main\n"),
        );

        let groups = pending_jobs(queue.path());
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[&PathBuf::from("/repos/one")].len(), 2);
        assert_eq!(groups[&PathBuf::from("/repos/two")].len(), 1);
        // The malformed job is dropped from the queue, the .tmp left untouched.
        assert!(!queue.path().join("d.job").exists());
        assert!(queue.path().join("ignored.tmp").exists());
    }

    // @relation(checks.sandbox, role=Verifies)
    #[test]
    fn backend_for_token_is_docker_without_a_token() {
        assert_eq!(backend_for_token(None), BackendKind::Docker);
    }

    // @relation(checks.sandbox, role=Verifies)
    #[test]
    fn backend_for_token_is_sprite_with_a_token() {
        assert_eq!(backend_for_token(Some("test-token")), BackendKind::Sprite);
    }

    // @relation(checks.sandbox, role=Verifies)
    #[test]
    fn docker_backend_runs_a_trivial_effect() {
        if docker::ensure_docker().is_err() {
            eprintln!("skipping docker_backend_runs_a_trivial_effect: docker is not available");
            return;
        }

        let repo = crate::testutil::unique_repo("docker-run");
        crate::testutil::write_effect_doc(&repo, "hello", "echo hi-from-docker");
        let status = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["commit", "--allow-empty", "-q", "-m", "seed"])
            .status()
            .unwrap();
        assert!(status.success());
        let head = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        assert!(head.status.success());
        let head = String::from_utf8(head.stdout).unwrap();

        let live = new_live_registry();
        let outcomes = run_effect_at(&repo, head.trim(), BackendKind::Docker, &live).unwrap();
        let outcome = outcomes
            .iter()
            .find(|outcome| outcome.name == "hello")
            .unwrap();
        assert_eq!(outcome.status, Status::Pass);
        assert!(
            outcome
                .recording
                .as_deref()
                .unwrap_or_default()
                .contains("hi-from-docker")
        );
    }
}
