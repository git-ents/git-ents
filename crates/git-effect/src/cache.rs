//! A read-write cache directory persisted at `refs/meta/cache/<name>`,
//! restored into the sandbox before an effect that names it runs and
//! snapshotted back after — unlike a toolchain (`git-toolchain`, extract-once
//! and immutable), a cache's contents change on every run, so it is written
//! back rather than only ever read.
//!
//! The persisted snapshot survives independent of the Sprite's own lifetime:
//! a Sprite reset or migration loses nothing a cache-using effect built up,
//! since the cache lives in the object database under [`CACHE_NS`] like
//! everything else `git-store` holds — not just on the Sprite's own
//! persistent filesystem, which the toolchain extraction cache leans on
//! instead.

use std::path::Path;
use std::process::Command;

use gix_hash::ObjectId;

/// The ref namespace holding cache snapshots, one ref per cache:
/// `refs/meta/cache/<name>`.
pub const CACHE_NS: &str = "refs/meta/cache";

/// The ref holding the cache named `name`.
#[must_use]
pub fn cache_ref(name: &str) -> String {
    format!("{CACHE_NS}/{name}")
}

/// Where cache `name` is restored inside the sandbox, exported to an
/// effect's command as `$EFFECT_CACHE_DIR`.
#[must_use]
pub fn cache_dir(name: &str) -> String {
    format!("/cache/{name}")
}

/// Restore `name`'s persisted snapshot (if any) into the sandbox at
/// [`cache_dir`], so an effect using it picks up where the last run against
/// this cache left off. The directory is created even when there is no prior
/// snapshot, so the tool populating it (sccache, ...) always finds it there
/// on a cold start.
///
/// ## Requirements
///
/// @relation(checks.cache)
pub fn restore(repo: &Path, sprite: &str, name: &str) -> Result<(), String> {
    let dir = cache_dir(name);
    let mkdir = Command::new("sprite")
        .args([
            "exec",
            "-s",
            sprite,
            "--",
            "sh",
            "-c",
            &format!("mkdir -p {dir}"),
        ])
        .status()
        .map_err(|e| format!("could not run the sprite CLI: {e}"))?;
    if !mkdir.success() {
        return Err(format!(
            "could not create cache directory {dir} in the sprite"
        ));
    }

    let store = git_store::Store::open(repo).map_err(|e| format!("could not open store: {e}"))?;
    let Ok(tree) = store.ref_tree(&cache_ref(name)) else {
        // No snapshot yet — a fresh cache, populated by whatever the command runs.
        return Ok(());
    };

    let mut archive = Command::new("git");
    archive
        .arg("-C")
        .arg(repo)
        .args(["archive", "--format=tar", &tree.to_string()]);
    let mut unpack = Command::new("sprite");
    unpack.args([
        "exec",
        "-s",
        sprite,
        "--",
        "sh",
        "-c",
        &format!("tar -x -C {dir}"),
    ]);
    crate::stream::pipe(archive, unpack, &format!("restoring cache {name}"))
}

/// Snapshot the sandbox's [`cache_dir`] for `name` back to [`cache_ref`],
/// replacing any prior snapshot with a parentless commit — cache history has
/// no audit value, so replaced snapshots become garbage-collectable instead
/// of pinned by a parent chain. The tip is the state [`restore`] will pick
/// up on this cache's next use.
///
/// ## Requirements
///
/// @relation(checks.cache)
pub fn snapshot(repo: &Path, sprite: &str, name: &str) -> Result<(), String> {
    let dir = cache_dir(name);
    let scratch = tempfile::tempdir().map_err(|e| format!("could not create temp dir: {e}"))?;
    let extracted = scratch.path().join("tree");
    std::fs::create_dir(&extracted).map_err(|e| format!("could not create extraction dir: {e}"))?;
    let mut archive = Command::new("sprite");
    archive.args([
        "exec",
        "-s",
        sprite,
        "--",
        "sh",
        "-c",
        &format!("tar -C {dir} -cf - ."),
    ]);
    let mut extract = Command::new("tar");
    extract.args(["-x", "-C"]).arg(&extracted);
    crate::stream::pipe(archive, extract, &format!("snapshotting cache {name}"))?;

    // A scratch index and an explicit work tree, so this builds a tree from
    // the extracted directory without disturbing the repository's own
    // (nonexistent, since it is bare) index. The index lives as a sibling of
    // `extracted`, never inside it — otherwise `git add -A .` stages the
    // index/lock files themselves, and a snapshot taken that way poisons every
    // future run: restoring it into the sandbox and archiving it back places
    // a stale `.git-index.lock` inside the next extraction, which then
    // collides with the real lock `git add` tries to create there.
    let index = scratch.path().join(".git-index");
    let add = Command::new("git")
        .arg("-C")
        .arg(repo)
        .env("GIT_INDEX_FILE", &index)
        .env("GIT_WORK_TREE", &extracted)
        .args(["add", "-A", "."])
        .status()
        .map_err(|e| format!("could not stage the cache tree: {e}"))?;
    if !add.success() {
        return Err(format!("could not stage cache {name}'s tree"));
    }
    let write_tree = Command::new("git")
        .arg("-C")
        .arg(repo)
        .env("GIT_INDEX_FILE", &index)
        .env("GIT_WORK_TREE", &extracted)
        .args(["write-tree"])
        .output()
        .map_err(|e| format!("could not write the cache tree: {e}"))?;
    if !write_tree.status.success() {
        return Err(format!("could not write cache {name}'s tree"));
    }
    let tree = String::from_utf8_lossy(&write_tree.stdout);
    let tree = ObjectId::from_hex(tree.trim().as_bytes())
        .map_err(|e| format!("git write-tree returned an invalid tree oid: {e}"))?;

    let store = git_store::Store::open(repo).map_err(|e| format!("could not open store: {e}"))?;
    store
        .store_tree_replace(&cache_ref(name), tree, "Update cache")
        .map_err(|e| format!("could not store cache {name}: {e}"))
}

/// [`restore`]'s local-backend equivalent: `dest` is already a host directory
/// (a [`crate::local::Sandbox`] bind-mounted straight into a Docker
/// container, or used as-is for host-direct execution), so restoring is just
/// extracting the snapshot tree onto it — no sandbox CLI transport needed.
///
/// ## Requirements
///
/// @relation(checks.cache)
pub fn restore_local(repo: &Path, dest: &Path, name: &str) -> Result<(), String> {
    std::fs::create_dir_all(dest).map_err(|e| format!("could not create cache directory: {e}"))?;

    let store = git_store::Store::open(repo).map_err(|e| format!("could not open store: {e}"))?;
    let Ok(tree) = store.ref_tree(&cache_ref(name)) else {
        return Ok(());
    };

    let mut archive = Command::new("git");
    archive
        .arg("-C")
        .arg(repo)
        .args(["archive", "--format=tar", &tree.to_string()]);
    let mut extract = Command::new("tar");
    extract.args(["-x", "-C"]).arg(dest);
    crate::stream::pipe(archive, extract, &format!("restoring cache {name}"))
}

/// [`snapshot`]'s local-backend equivalent: `src` is already a host
/// directory, so snapshotting is building a tree from it directly, without
/// first archiving it out of a sandbox. Uses a scratch index alongside `src`
/// (never inside it) for the same reason [`snapshot`] does: a `.git-index`
/// left inside `src` would poison the next run's restore/snapshot cycle.
///
/// ## Requirements
///
/// @relation(checks.cache)
pub fn snapshot_local(repo: &Path, src: &Path, name: &str) -> Result<(), String> {
    let index_dir =
        tempfile::tempdir().map_err(|e| format!("could not create scratch dir: {e}"))?;
    let index = index_dir.path().join(".git-index");

    let add = Command::new("git")
        .arg("-C")
        .arg(repo)
        .env("GIT_INDEX_FILE", &index)
        .env("GIT_WORK_TREE", src)
        .args(["add", "-A", "."])
        .status()
        .map_err(|e| format!("could not stage cache {name}'s tree: {e}"))?;
    if !add.success() {
        return Err(format!("could not stage cache {name}'s tree"));
    }
    let write_tree = Command::new("git")
        .arg("-C")
        .arg(repo)
        .env("GIT_INDEX_FILE", &index)
        .env("GIT_WORK_TREE", src)
        .args(["write-tree"])
        .output()
        .map_err(|e| format!("could not write cache {name}'s tree: {e}"))?;
    if !write_tree.status.success() {
        return Err(format!("could not write cache {name}'s tree"));
    }
    let tree = String::from_utf8_lossy(&write_tree.stdout);
    let tree = ObjectId::from_hex(tree.trim().as_bytes())
        .map_err(|e| format!("git write-tree returned an invalid tree oid: {e}"))?;

    let store = git_store::Store::open(repo).map_err(|e| format!("could not open store: {e}"))?;
    store
        .store_tree_replace(&cache_ref(name), tree, "Update cache")
        .map_err(|e| format!("could not store cache {name}: {e}"))
}
