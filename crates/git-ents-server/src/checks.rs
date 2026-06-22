//! The `post-receive` check runner: a git hook that runs the configured checks
//! against a push inside a Fly.io [Sprite].
//!
//! Where the `pre-receive` verifier gates the push synchronously, checks run
//! *after* the refs are in. The runner reads the pushed ref updates git feeds
//! the hook on stdin, loads the check set from `refs/meta/checks`, and for each
//! updated branch runs every check in a Sprite — a persistent, hardware-isolated
//! sandbox. One Sprite is kept per repository so its filesystem (and any build
//! cache a check leaves behind) survives between pushes; the pushed tree is
//! synced into it before the checks run. Results are reported on the hook's
//! stdout, which git relays to the pusher.
//!
//! The Sprite is driven through the `sprite` CLI, which reads its `SPRITES_TOKEN`
//! from the environment the server passes down to the hook.
//!
//! [Sprite]: https://sprites.dev

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use git_ents::checks::{self, Check, RunOutcome};

/// Where the pushed tree is unpacked inside the Sprite.
const WORKDIR: &str = "/work";

/// Run the configured checks against the push git is reporting, returning
/// `Ok(())` once results have been printed. The ref updates are read from the
/// stdin git populates for a `post-receive` hook (`<old> <new> <ref>` lines).
///
/// A `post-receive` exit code cannot undo refs that are already in, so a failed
/// check is reported rather than turned into an error: the function returns
/// `Err` only when the runner itself could not run (unreadable check set, an
/// unreachable Sprite), never merely because a check failed.
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

    let checks = checks::load(&repo).map_err(|e| format!("could not read checks: {e}"))?;
    if checks.is_empty() {
        return Ok(());
    }

    let sprite = sprite_name(&repo);
    ensure_sprite(&sprite)?;

    for update in updates {
        println!(
            "checks: running {} check(s) on {}",
            checks.len(),
            update.ref_name
        );
        sync_tree(&repo, &sprite, update.new)?;
        let outcomes = run_checks(&sprite, &checks);
        // Persist the run as a ref (`refs/checks/<commit>`); a recording hiccup
        // is reported but never fails the hook.
        if let Err(e) = checks::record(&repo, update.new, &outcomes) {
            eprintln!("checks: could not record run for {}: {e}", update.new);
        }
    }
    Ok(())
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

/// Run each check in the Sprite's [`WORKDIR`], printing a `PASS`/`FAIL` line per
/// check and echoing the output of any that fail so the pusher sees why. Returns
/// each check's outcome (`pass`/`fail`/`error`) for recording.
fn run_checks(sprite: &str, checks: &[Check]) -> Vec<RunOutcome> {
    let mut outcomes = Vec::with_capacity(checks.len());
    for check in checks {
        let output = Command::new("sprite")
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
            .output();
        let outcome = match output {
            Ok(output) if output.status.success() => {
                println!("checks: PASS {}", check.name);
                "pass"
            }
            Ok(output) => {
                println!("checks: FAIL {} ({})", check.name, check.command);
                let logs = String::from_utf8_lossy(&output.stderr);
                let logs = if logs.trim().is_empty() {
                    String::from_utf8_lossy(&output.stdout)
                } else {
                    logs
                };
                for line in logs.lines() {
                    println!("checks:   {line}");
                }
                "fail"
            }
            Err(e) => {
                println!("checks: ERROR {} (could not run: {e})", check.name);
                "error"
            }
        };
        outcomes.push(RunOutcome {
            name: check.name.clone(),
            outcome: outcome.to_owned(),
        });
    }
    outcomes
}
