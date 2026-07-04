//! Asynchronous check running: a `post-receive` hook that *queues* a push and a
//! server-owned worker that runs the configured checks against it in a Fly.io
//! [Sprite].
//!
//! Checks run *after* the refs are in and off the push connection. The hook
//! ([`post_receive`]) does almost nothing: it reads the pushed ref updates git
//! feeds it on stdin and drops a job file into the shared queue directory, so
//! the push returns immediately. The long-running server drains that queue from
//! a dedicated worker ([`worker`]); for each job it loads the check set from
//! `refs/meta/checks` and runs every check in a Sprite — a persistent,
//! hardware-isolated sandbox. One Sprite is kept per repository so its
//! filesystem (and any build cache a check leaves behind) survives between
//! pushes; the pushed tree is synced into it before the checks run. Results are
//! recorded as run refs (and surfaced on the Checks tab), and logged to the
//! server's own output rather than relayed to the pusher.
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

use git_ents_core::checks::{self, Check, RunOutcome, Status};
use gix_hash::ObjectId;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use tokio::sync::Mutex;

/// Where the pushed tree is unpacked inside the Sprite.
const WORKDIR: &str = "/work";

/// Where resolved toolchains are extracted inside the Sprite, one directory
/// per tree hash (`{TOOLCHAINS_DIR}/<hash>`) — unlike [`WORKDIR`], never
/// cleared: the Sprite's persistent filesystem is the extract-once cache.
const TOOLCHAINS_DIR: &str = "/toolchains";

/// A currently-running check's growing asciicast v2 recording, keyed by the
/// repository, the commit being checked, and the check's name.
pub(crate) type LiveKey = (PathBuf, ObjectId, String);

/// Live buffers for every check currently running, shared between the worker
/// thread appending to a check's output as it arrives and the web layer
/// polling it for a live view. A buffer exists only while its check is
/// running — [`live_start`] adds it, [`live_finish`] removes it once the
/// result is recorded — so a lookup miss unambiguously means "not running"
/// rather than "running with no output yet". Asciicast is the definitive log
/// format end to end: the same string a live poll reads is, unmodified,
/// what [`run_one`] hands back as the check's recorded `recording`.
pub(crate) type LiveRegistry = Arc<StdMutex<HashMap<LiveKey, Arc<StdMutex<String>>>>>;

/// A fresh, empty [`LiveRegistry`] — one per server process, held on
/// [`crate::AppState`].
pub(crate) fn new_live_registry() -> LiveRegistry {
    Arc::new(StdMutex::new(HashMap::new()))
}

/// The text accumulated so far for a running check's live buffer, or `None`
/// when no check is running under `key` (finished, or never started).
pub(crate) fn live_snapshot(registry: &LiveRegistry, key: &LiveKey) -> Option<String> {
    let buffer = lock(registry).get(key).cloned()?;
    Some(lock(&buffer).clone())
}

/// Register a fresh live buffer for `key`, returning the handle [`run_one`]
/// appends to as the check's output arrives.
fn live_start(registry: &LiveRegistry, key: LiveKey) -> Arc<StdMutex<String>> {
    let buffer = Arc::new(StdMutex::new(String::new()));
    lock(registry).insert(key, Arc::clone(&buffer));
    buffer
}

/// Remove `key`'s live buffer once its check has settled — recorded results
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

/// Queue the push git is reporting for asynchronous checking, returning `Ok(())`
/// once the jobs are enqueued. The ref updates are read from the stdin git
/// populates for a `post-receive` hook (`<old> <new> <ref>` lines).
///
/// The hook does no check work itself: it writes one job file per updated branch
/// into the shared queue directory ([`QUEUE_ENV`]) and returns, so the push is
/// never blocked on a Sprite. The server's [`worker`] picks the jobs up.
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

    // An empty check set leaves nothing to queue.
    let runnable = checks::load(&repo).map_err(|e| format!("could not read checks: {e}"))?;
    if runnable.is_empty() {
        return Ok(());
    }

    let Some(queue) = std::env::var_os(QUEUE_ENV).map(PathBuf::from) else {
        eprintln!("checks: {QUEUE_ENV} is not set; skipping asynchronous checks");
        return Ok(());
    };

    for update in updates {
        enqueue(&queue, &repo, &update)?;
        // Record the run as `queued` straight away so it shows up on the Checks
        // tab the moment the push lands, before the worker picks it up; a
        // recording hiccup is reported but never fails the hook.
        let queued = statuses(&runnable, Status::Queued);
        if let Err(e) = checks::record(&repo, update.new, &queued) {
            eprintln!(
                "checks: could not record queued run for {}: {e}",
                update.new
            );
        }
        println!(
            "checks: queued {} check(s) on {}",
            runnable.len(),
            update.ref_name
        );
    }
    Ok(())
}

/// Every check's [`RunOutcome`] set to one shared `status` — the queued/running
/// snapshot a run starts from before per-check results land.
fn statuses(checks: &[Check], status: Status) -> Vec<RunOutcome> {
    checks
        .iter()
        .map(|check| RunOutcome {
            name: check.name.clone(),
            status,
            duration_secs: None,
            recording: None,
            exit_code: None,
        })
        .collect()
}

/// Run the worker that drains the queue directory, running and recording the
/// checks for each queued push. Runs for the life of the server; the blocking
/// Sprite work is offloaded so it never stalls the async runtime.
///
/// Jobs are processed per repository: each tick, every repository with pending
/// jobs that is not already being worked gets its own blocking task that drains
/// its jobs in order. Because a check can run up to [`CHECK_TIMEOUT`], serving
/// all repositories from one queue scan would let a single slow repository stall
/// every other repository's checks; isolating them by repository keeps a slow
/// repository's backlog from blocking the rest. Jobs for *one* repository stay
/// serialized so concurrent runs never collide in its single Sprite.
pub async fn worker(queue: PathBuf, live: LiveRegistry) {
    if let Err(e) = std::fs::create_dir_all(&queue) {
        eprintln!("checks: could not create queue directory {queue:?}: {e}");
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
            let handle = tokio::task::spawn_blocking(move || drain_repo(&jobs, &live));
            tokio::spawn(async move {
                let _done = handle.await;
                inflight.lock().await.remove(&repo);
            });
        }
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
fn drain_repo(jobs: &[(PathBuf, Job)], live: &LiveRegistry) {
    for (path, job) in jobs {
        if let Err(e) = process_job(job, live) {
            eprintln!("checks: {e}");
        }
        let _removed = std::fs::remove_file(path);
    }
}

/// Run the checks for one queued push in its repository's Sprite, advancing the
/// recorded run as it goes: `running` while the Sprite is prepared, then each
/// check flipped to its result as it finishes. Checks settle in the dependency
/// order `checks::order` fixed at write time: a check whose dependency did not
/// pass is recorded `skipped` without touching the Sprite, and a composite (no
/// command) derives its status from its dependencies alone. An infra failure
/// (an unreachable Sprite, a tree that will not sync, a check set that fails
/// re-validation) finalizes the run as `error` rather than leaving it stuck at
/// `running`, then returns `Err`. Returns `Ok` even when a check fails — a
/// failing check is a recorded result, not an error.
fn process_job(job: &Job, live: &LiveRegistry) -> Result<(), String> {
    let runnable = checks::load(&job.repo).map_err(|e| format!("could not read checks: {e}"))?;
    if runnable.is_empty() {
        return Ok(());
    }

    let mut outcomes = statuses(&runnable, Status::Running);
    // Re-validate defensively: the CLI rejects an invalid graph before it is
    // pushed, but a hand-crafted push could still land one. Indices into
    // `runnable`/`outcomes` rather than borrows, so outcomes stay mutable.
    let ordered: Vec<usize> = match checks::order(&runnable) {
        Ok(ordered) => ordered
            .iter()
            .filter_map(|check| runnable.iter().position(|c| c.name == check.name))
            .collect(),
        Err(e) => {
            finalize_error(&job.repo, job.new, &mut outcomes);
            return Err(format!("invalid check set: {e}"));
        }
    };
    let sprite = sprite_name(&job.repo);
    if let Err(e) = ensure_auth().and_then(|()| ensure_sprite(&sprite)) {
        finalize_error(&job.repo, job.new, &mut outcomes);
        return Err(e);
    }

    eprintln!(
        "checks: running {} check(s) on {}",
        runnable.len(),
        job.ref_name
    );
    advance(&job.repo, job.new, &outcomes);
    if let Err(e) = sync_tree(&job.repo, &sprite, job.new) {
        finalize_error(&job.repo, job.new, &mut outcomes);
        return Err(e);
    }

    let toolchain_dirs = match resolve_toolchains(&job.repo, &sprite, &runnable) {
        Ok(dirs) => dirs,
        Err(e) => {
            finalize_error(&job.repo, job.new, &mut outcomes);
            return Err(e);
        }
    };

    for index in ordered {
        let Some(check) = runnable.get(index) else {
            continue;
        };
        // Topological order guarantees every dependency settled already.
        let deps: Vec<Status> = check
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
        match &check.command {
            Some(command) if all_pass => {
                let command = activate(command, &check.toolchains, &toolchain_dirs);
                let key: LiveKey = (job.repo.clone(), job.new, check.name.clone());
                let buffer = live_start(live, key.clone());
                let result = run_one(&sprite, &check.name, &command, &buffer);
                live_finish(live, &key);
                if let Some(outcome) = outcomes.get_mut(index) {
                    outcome.status = result.status;
                    outcome.duration_secs = Some(result.duration_secs);
                    outcome.recording = Some(result.recording);
                    outcome.exit_code = result.exit_code;
                }
            }
            Some(_) => {
                eprintln!("checks: SKIP {} (a dependency did not pass)", check.name);
                if let Some(outcome) = outcomes.get_mut(index) {
                    outcome.status = Status::Skipped;
                }
            }
            None => {
                let status = derive_composite(&deps);
                eprintln!(
                    "checks: {} {} (composite)",
                    status.to_string().to_uppercase(),
                    check.name
                );
                if let Some(outcome) = outcomes.get_mut(index) {
                    outcome.status = status;
                }
            }
        }
        advance(&job.repo, job.new, &outcomes);
    }
    Ok(())
}

/// A composite check's status, derived from its dependencies' settled
/// statuses: `pass` when everything passed, `fail` when anything failed or
/// errored, `skipped` when nothing failed but something was skipped.
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
    if let Err(e) = checks::update_run(repo, new, outcomes) {
        eprintln!("checks: could not record run for {new}: {e}");
    }
}

/// Mark every check in `outcomes` `error` and record it — the terminal state for
/// a run the worker could not carry out.
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

/// Parse git's `<old-oid> <new-oid> <ref>` stdin into the updates worth
/// checking: branch updates with a real new tip. Deletions (a zero new oid) and
/// the `refs/meta/*` control refs (auth, the check set itself) are skipped — the
/// checks gate ordinary content, not the trust plumbing.
fn parse_updates(input: &str) -> Vec<Update<'_>> {
    input
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let _old = fields.next()?;
            let new = fields.next()?;
            let ref_name = fields.next()?;
            let new = ObjectId::from_hex(new.as_bytes()).ok()?;
            if new.is_null() || ref_name.starts_with("refs/meta/") {
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
/// Shared with [`crate::web`]'s debug-session broker, which targets the same
/// persistent per-repo Sprite a check run used.
pub(crate) fn sprite_name(repo: &Path) -> String {
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
pub(crate) fn ensure_auth() -> Result<(), String> {
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
pub(crate) fn ensure_sprite(sprite: &str) -> Result<(), String> {
    let _existing = Command::new("sprite")
        .args(["create", "--skip-console", sprite])
        .output()
        .map_err(|e| format!("could not run the sprite CLI (is it installed?): {e}"))?;
    Ok(())
}

/// Stream the pushed tree at `new` into the Sprite's [`WORKDIR`], replacing any
/// previous contents while leaving the rest of the persistent filesystem (build
/// caches and the like) intact. `git archive` emits the tree as a tar that the
/// Sprite unpacks over stdin.
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

    let script = format!("rm -rf {WORKDIR} && mkdir -p {WORKDIR} && tar -x -C {WORKDIR}");
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

/// Resolve and extract every distinct toolchain named across `runnable`,
/// returning each name's extracted `bin` directory inside the Sprite. A
/// failed resolution (the named ref does not exist) is the one place
/// `checks::order` could not have caught it, since `refs/meta/toolchains/*`
/// is a different namespace than the check set itself.
fn resolve_toolchains(
    repo: &Path,
    sprite: &str,
    runnable: &[Check],
) -> Result<HashMap<String, String>, String> {
    let mut names: Vec<&str> = runnable
        .iter()
        .flat_map(|check| check.toolchains.iter().map(String::as_str))
        .collect();
    names.sort_unstable();
    names.dedup();

    let mut dirs = HashMap::new();
    for name in names {
        let toolchain = git_toolchain::resolve(repo, name)
            .map_err(|e| format!("could not resolve toolchain {name}: {e}"))?;
        let bin = toolchain.bin.oid();
        sync_toolchain(repo, sprite, bin)?;
        dirs.insert(name.to_owned(), format!("{TOOLCHAINS_DIR}/{bin}"));
    }
    Ok(dirs)
}

/// Prefix `command` with a `PATH` export activating `toolchains`' extracted
/// `bin` directories, declared order first (so the first-listed toolchain's
/// `bin` wins on a name collision); a check with no toolchains is returned
/// unchanged.
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

/// Extract the toolchain tree `tree` into the Sprite at
/// `{TOOLCHAINS_DIR}/<tree>`, once — a directory already there from an
/// earlier push is left alone rather than re-extracted, since the Sprite's
/// persistent filesystem is the cache. Checked before running `git archive`
/// so an already-cached toolchain never streams its (potentially large)
/// contents through a pipe the Sprite has no reason to read.
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

    let script = format!("mkdir -p {dir} && tar -x -C {dir}");
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

/// How long a single check may run before the worker abandons it. A runaway
/// check that outlived this — a hung build, a command blocked on input — is
/// killed and recorded `error` rather than wedging the worker (and with it every
/// other repository's checks) on the one blocking-pool thread the queue drains
/// on.
const CHECK_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// The fixed size a check's recorded terminal session runs at. Nothing
/// interactive ever attaches to it, so this only shapes the recording, not
/// anyone's actual terminal.
const CHECK_PTY_SIZE: PtySize = PtySize {
    rows: 24,
    cols: 80,
    pixel_width: 0,
    pixel_height: 0,
};

/// A finished check run: its outcome, wall-clock duration, process exit code
/// (when the command ran to completion), and the full terminal session as an
/// asciicast v2 recording.
struct RunResult {
    status: Status,
    duration_secs: u64,
    recording: String,
    exit_code: Option<i32>,
}

/// Run one check in the Sprite's [`WORKDIR`], recording its terminal session —
/// a real pty (`sprite exec --tty`), not a pipe, so the recording plays back
/// exactly what a developer running the check by hand would see — and logging
/// a `PASS`/`FAIL` line. `live` is appended to as output arrives, in the same
/// asciicast v2 format as the final recording, so a browser can poll it for a
/// live view of a check still in progress; it is what [`finish`] hands back
/// as the recorded `recording`, not a separate representation of the same
/// output. Returns the check's outcome; a check that exceeds [`CHECK_TIMEOUT`]
/// or cannot be captured is [`Status::Error`].
fn run_one(sprite: &str, name: &str, command: &str, live: &Arc<StdMutex<String>>) -> RunResult {
    let start = Instant::now();
    lock(live).push_str(&asciicast_header());

    let pair = match native_pty_system().openpty(CHECK_PTY_SIZE) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("checks: ERROR {name} (could not allocate a pty: {e})");
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
            eprintln!("checks: ERROR {name} (could not run: {e})");
            return finish(Status::Error, start, None, live);
        }
    };
    // The child holds the slave now; drop ours so the master sees EOF when the
    // check process actually exits rather than when this scope happens to end.
    drop(pair.slave);

    let master = pair.master;
    let Ok(mut reader) = master.try_clone_reader() else {
        eprintln!("checks: ERROR {name} (could not read the pty)");
        let _killed = child.kill();
        return finish(Status::Error, start, None, live);
    };

    // The pty's `Read` is blocking, so it gets its own thread; the main thread
    // times the whole run out against [`CHECK_TIMEOUT`] by bounding how long it
    // waits on the channel rather than the read itself.
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
    let timed_out = loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            break true;
        };
        match rx.recv_timeout(remaining) {
            Ok(chunk) => {
                let elapsed = start.elapsed().as_secs_f64();
                let data = String::from_utf8_lossy(&chunk);
                push_event(&mut lock(live), elapsed, &data);
            }
            Err(RecvTimeoutError::Timeout) => break true,
            Err(RecvTimeoutError::Disconnected) => break false,
        }
    };
    drop(master);

    if timed_out {
        eprintln!("checks: ERROR {name} (timed out after {CHECK_TIMEOUT:?})");
        let _killed = child.kill();
        return finish(Status::Error, start, None, live);
    }

    let status = match child.wait() {
        Ok(status) => status,
        Err(e) => {
            eprintln!("checks: ERROR {name} (could not wait on the sprite CLI: {e})");
            return finish(Status::Error, start, None, live);
        }
    };

    let exit_code = Some(i32::try_from(status.exit_code()).unwrap_or(i32::MAX));
    if status.success() {
        eprintln!("checks: PASS {name}");
        finish(Status::Pass, start, exit_code, live)
    } else {
        eprintln!("checks: FAIL {name} ({command})");
        finish(Status::Fail, start, exit_code, live)
    }
}

/// Assemble a [`RunResult`] from `live`'s accumulated recording — used on
/// every exit path, including the failure ones, so a check that errors out
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
/// — the first line of every check recording, live or finished (see
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

    #[test]
    fn parse_updates_keeps_content_branches_only() {
        let new = "1111111111111111111111111111111111111111";
        let input = format!(
            "{zero} {new} refs/heads/main\n\
             {new} {zero} refs/heads/old\n\
             {new} {new} refs/meta/checks\n\
             {new} {new} refs/heads/feature\n",
            zero = git_ents_core::ZERO_OID,
        );
        let updates = parse_updates(&input);
        let refs: Vec<&str> = updates.iter().map(|u| u.ref_name).collect();
        assert_eq!(refs, vec!["refs/heads/main", "refs/heads/feature"]);
    }

    #[test]
    fn activate_leaves_a_toolchain_free_command_unchanged() {
        let dirs = HashMap::new();
        assert_eq!(activate("cargo test", &[], &dirs), "cargo test");
    }

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

    #[test]
    fn activate_skips_a_toolchain_missing_from_dirs() {
        let dirs = HashMap::new();
        let toolchains = vec!["gcc".to_owned()];
        assert_eq!(
            activate("make", &toolchains, &dirs),
            "export PATH=:$PATH; make"
        );
    }

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
}
