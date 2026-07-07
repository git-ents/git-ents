//! A minimal best-effort [`RefEventStream`] source: a background thread
//! that polls the ref namespace's on-disk footprint and sends a wakeup
//! hint on change. Local loose refs have no push notification channel to
//! hook into, so polling is the whole mechanism — acceptable per `watch`'s
//! contract, which only promises a hint, never delivery.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use git_backend::{Error, RefEvent, RefEventStream, Result};

/// How often the background thread re-checks the watched prefix's on-disk
/// footprint.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Start polling `git_dir` for changes under `prefix` and return the
/// [`RefEventStream`] that receives a hint on every detected change. The
/// background thread exits on its own once the stream (and its sender) is
/// dropped.
pub fn spawn(git_dir: PathBuf, prefix: String) -> Result<RefEventStream> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("refstore-files-watch".to_owned())
        .spawn(move || poll_loop(&git_dir, &prefix, &tx))
        .map_err(Error::Io)?;
    Ok(RefEventStream::new(rx))
}

/// Loop until the receiving end of `tx` is dropped, sending a [`RefEvent`]
/// whenever `fingerprint` changes.
fn poll_loop(git_dir: &Path, prefix: &str, tx: &std::sync::mpsc::Sender<RefEvent>) {
    let mut last = fingerprint(git_dir, prefix);
    loop {
        std::thread::sleep(POLL_INTERVAL);
        let current = fingerprint(git_dir, prefix);
        if current != last {
            last = current;
            if tx.send(RefEvent).is_err() {
                return;
            }
        }
    }
}

/// A cheap, approximate signature of every ref under `prefix`: the newest
/// modification time and the count of files considered, across both loose
/// refs and `packed-refs`. Good enough for a wakeup hint — an exact match
/// is not the contract, only "something changed since last time".
fn fingerprint(git_dir: &Path, prefix: &str) -> (Option<SystemTime>, u64) {
    let mut newest = None;
    let mut count: u64 = 0;
    {
        let mut visit = |path: &Path| {
            let Ok(metadata) = std::fs::metadata(path) else {
                return;
            };
            let Ok(modified) = metadata.modified() else {
                return;
            };
            count = count.saturating_add(1);
            if newest.is_none_or(|previous| modified > previous) {
                newest = Some(modified);
            }
        };
        visit(&git_dir.join("packed-refs"));
        walk(&git_dir.join("refs"), prefix, git_dir, &mut visit);
    }
    (newest, count)
}

/// Recursively visit every regular file under `dir`, calling `visit` on
/// those whose path relative to `git_dir` starts with `prefix` — the loose
/// ref files a change under `prefix` would touch.
fn walk(dir: &Path, prefix: &str, git_dir: &Path, visit: &mut impl FnMut(&Path)) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            walk(&path, prefix, git_dir, visit);
        } else if file_type.is_file() {
            let relative = path.strip_prefix(git_dir).unwrap_or(&path);
            if relative.to_string_lossy().starts_with(prefix) {
                visit(&path);
            }
        }
    }
}
