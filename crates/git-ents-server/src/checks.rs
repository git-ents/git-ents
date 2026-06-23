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

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use git_ents::checks::{self, Check, RunOutcome};

/// Where the pushed tree is unpacked inside the Sprite.
const WORKDIR: &str = "/work";

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
        let queued = statuses(&runnable, "queued");
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
fn statuses(checks: &[Check], status: &str) -> Vec<RunOutcome> {
    checks
        .iter()
        .map(|check| RunOutcome {
            name: check.name.clone(),
            outcome: status.to_owned(),
        })
        .collect()
}

/// Run the worker that drains the queue directory, running and recording the
/// checks for each queued push. Runs for the life of the server; the blocking
/// Sprite work is offloaded so it never stalls the async runtime.
pub async fn worker(queue: PathBuf) {
    if let Err(e) = std::fs::create_dir_all(&queue) {
        eprintln!("checks: could not create queue directory {queue:?}: {e}");
        return;
    }
    let mut tick = tokio::time::interval(POLL);
    loop {
        tick.tick().await;
        let queue = queue.clone();
        let _drained = tokio::task::spawn_blocking(move || drain(&queue)).await;
    }
}

/// Process every job currently in the queue directory once, deleting each job
/// file after it is handled (whether it ran cleanly, failed, or was malformed) —
/// a poison job is dropped rather than retried forever.
fn drain(queue: &Path) {
    let Ok(entries) = std::fs::read_dir(queue) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "job") {
            continue;
        }
        if let Some(job) = read_job(&path)
            && let Err(e) = process_job(&job)
        {
            eprintln!("checks: {e}");
        }
        let _removed = std::fs::remove_file(&path);
    }
}

/// Run the checks for one queued push in its repository's Sprite, advancing the
/// recorded run as it goes: `running` while the Sprite is prepared, then each
/// check flipped to its result as it finishes. An infra failure (an unreachable
/// Sprite, a tree that will not sync) finalizes the run as `error` rather than
/// leaving it stuck at `running`, then returns `Err`. Returns `Ok` even when a
/// check fails — a failing check is a recorded result, not an error.
fn process_job(job: &Job) -> Result<(), String> {
    let runnable = checks::load(&job.repo).map_err(|e| format!("could not read checks: {e}"))?;
    if runnable.is_empty() {
        return Ok(());
    }

    let mut outcomes = statuses(&runnable, "running");
    let sprite = sprite_name(&job.repo);
    if let Err(e) = ensure_auth().and_then(|()| ensure_sprite(&sprite)) {
        finalize_error(&job.repo, &job.new, &mut outcomes);
        return Err(e);
    }

    eprintln!(
        "checks: running {} check(s) on {}",
        runnable.len(),
        job.ref_name
    );
    advance(&job.repo, &job.new, &outcomes);
    if let Err(e) = sync_tree(&job.repo, &sprite, &job.new) {
        finalize_error(&job.repo, &job.new, &mut outcomes);
        return Err(e);
    }

    for (index, check) in runnable.iter().enumerate() {
        let result = run_one(&sprite, check);
        if let Some(outcome) = outcomes.get_mut(index) {
            outcome.outcome = result.to_owned();
        }
        advance(&job.repo, &job.new, &outcomes);
    }
    Ok(())
}

/// Advance the recorded run for `new` to `outcomes`; a recording hiccup is
/// logged but never derails the worker.
fn advance(repo: &Path, new: &str, outcomes: &[RunOutcome]) {
    if let Err(e) = checks::update_run(repo, new, outcomes) {
        eprintln!("checks: could not record run for {new}: {e}");
    }
}

/// Mark every check in `outcomes` `error` and record it — the terminal state for
/// a run the worker could not carry out.
fn finalize_error(repo: &Path, new: &str, outcomes: &mut [RunOutcome]) {
    for outcome in outcomes.iter_mut() {
        outcome.outcome = "error".to_owned();
    }
    advance(repo, new, outcomes);
}

/// One queued push: the repository to check, the new tip to check, and the ref
/// it updated (carried only for logging).
struct Job {
    repo: PathBuf,
    new: String,
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

/// A unique job file stem (`<nanos>-<pid>-<counter>`) so concurrent pushes never
/// collide on a queue file name.
fn job_stem() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{nanos}-{}-{n}", std::process::id())
}

/// Parse a queued job file (`repo`, new oid, ref, one per line), or `None` when
/// it is malformed.
fn read_job(path: &Path) -> Option<Job> {
    let contents = std::fs::read_to_string(path).ok()?;
    let mut lines = contents.lines();
    let repo = PathBuf::from(lines.next()?);
    let new = lines.next()?.to_owned();
    let ref_name = lines.next()?.to_owned();
    if new.is_empty() {
        return None;
    }
    Some(Job {
        repo,
        new,
        ref_name,
    })
}

/// One ref git reported as updated by the push.
struct Update<'a> {
    new: &'a str,
    ref_name: &'a str,
}

/// Parse git's `<old-oid> <new-oid> <ref>` stdin into the updates worth
/// checking: branch updates with a real new tip. Deletions (a zero new oid) and
/// the `refs/meta/*` control refs (auth, the check set itself) are skipped — the
/// checks gate ordinary content, not the trust plumbing.
fn parse_updates(input: &str) -> Vec<Update<'_>> {
    const ZERO: &str = "0000000000000000000000000000000000000000";
    input
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let _old = fields.next()?;
            let new = fields.next()?;
            let ref_name = fields.next()?;
            if new == ZERO || ref_name.starts_with("refs/meta/") {
                None
            } else {
                Some(Update { new, ref_name })
            }
        })
        .collect()
}

/// A Sprite name derived from the repository directory, kept to the
/// `[a-z0-9-]` a Sprite name allows so the same repo reuses the same sandbox.
fn sprite_name(repo: &Path) -> String {
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
fn ensure_auth() -> Result<(), String> {
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
fn ensure_sprite(sprite: &str) -> Result<(), String> {
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
fn sync_tree(repo: &Path, sprite: &str, new: &str) -> Result<(), String> {
    let archive = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["archive", "--format=tar", new])
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

/// How long a single check may run before the worker abandons it. A runaway
/// check that outlived this — a hung build, a command blocked on input — is
/// killed and recorded `error` rather than wedging the worker (and with it every
/// other repository's checks) on the one blocking-pool thread the queue drains
/// on.
const CHECK_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Run one check in the Sprite's [`WORKDIR`], logging a `PASS`/`FAIL` line and
/// echoing the output on failure. Returns its outcome (`pass`/`fail`/`error`); a
/// check that exceeds [`CHECK_TIMEOUT`] or cannot be captured is `error`.
fn run_one(sprite: &str, check: &Check) -> &'static str {
    let child = Command::new("sprite")
        .args([
            "exec",
            "-s",
            sprite,
            "--dir",
            WORKDIR,
            "--",
            "sh",
            "-c",
            &check.command,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let child = match child {
        Ok(child) => child,
        Err(e) => {
            eprintln!("checks: ERROR {} (could not run: {e})", check.name);
            return "error";
        }
    };
    let Some(output) = wait_bounded(child, CHECK_TIMEOUT) else {
        eprintln!(
            "checks: ERROR {} (timed out after {:?} or could not be captured)",
            check.name, CHECK_TIMEOUT
        );
        return "error";
    };
    if output.status.success() {
        eprintln!("checks: PASS {}", check.name);
        "pass"
    } else {
        eprintln!("checks: FAIL {} ({})", check.name, check.command);
        let logs = String::from_utf8_lossy(&output.stderr);
        let logs = if logs.trim().is_empty() {
            String::from_utf8_lossy(&output.stdout)
        } else {
            logs
        };
        for line in logs.lines() {
            eprintln!("checks:   {line}");
        }
        "fail"
    }
}

/// Wait up to `timeout` for `child` to finish, returning its captured output, or
/// `None` if it timed out or could not be waited on. On timeout the process is
/// killed by pid — `sprite exec` is a local proxy for the remote command, so
/// killing it frees the worker even though the in-Sprite command may run on.
fn wait_bounded(child: std::process::Child, timeout: Duration) -> Option<std::process::Output> {
    let pid = child.id();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _sent = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok(output)) => Some(output),
        Ok(Err(_failed)) => None,
        Err(_timeout) => {
            let _killed = Command::new("kill").args(["-9", &pid.to_string()]).status();
            None
        }
    }
}
