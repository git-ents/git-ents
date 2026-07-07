//! Shared fixtures: distinct real commit oids, and a valid pack built the
//! same way a push transmits one. Property functions exercise backends
//! against real git object bytes rather than synthetic hashes, since a
//! backend is free to validate what it's handed and no real pack could
//! ever contain a made-up hash's "content".

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "fixture helpers for a conformance suite, not application code"
)]

use std::path::Path;
use std::process::{Command, Stdio};

use git_store::test_support::{commit_all, head, repo};
use gix_hash::ObjectId;

/// `n` distinct, real commit object ids, built by committing `n` times in
/// a throwaway repository unrelated to any backend under test. Only usable
/// as `RefEdit` targets for a `RefStore` that never dereferences into
/// object storage (e.g. a Postgres-backed one); a backend that resolves a
/// ref by reading its target object needs [`commit_oids_into`] instead,
/// since this repository is not the one it reads from.
pub fn distinct_oids(n: usize) -> Vec<ObjectId> {
    let dir = repo();
    commit_oids_into(dir.path(), n)
}

/// `n` distinct, real commit object ids, built by committing `n` times
/// into the already-initialized repository at `path`. For a `RefStore`
/// backend that peels a ref by reading its target object (gitoxide-backed
/// ones do), `path` must be the same repository the backend was opened
/// against, so the objects a `RefEdit` points at actually resolve.
pub fn commit_oids_into(path: &Path, n: usize) -> Vec<ObjectId> {
    (0..n)
        .map(|i| {
            std::fs::write(path.join("file"), i.to_string()).expect("write fixture file");
            commit_all(path, &format!("conformance fixture {i}"));
            let hex = head(path);
            ObjectId::from_hex(hex.as_bytes()).expect("valid oid hex")
        })
        .collect()
}

/// A real commit oid, and a pack containing it and everything it reaches.
pub struct PackFixture {
    /// The commit at the tip of [`PackFixture::pack`].
    pub oid: ObjectId,
    /// A pack containing `oid` and everything it reaches.
    pub pack: Vec<u8>,
}

/// Build a [`PackFixture`]: one commit in a fresh throwaway repository,
/// packed on its own.
pub fn oid_and_pack() -> PackFixture {
    let dir = repo();
    std::fs::write(dir.path().join("file"), b"content").expect("write fixture file");
    commit_all(dir.path(), "conformance fixture");
    let hex = head(dir.path());
    let oid = ObjectId::from_hex(hex.as_bytes()).expect("valid oid hex");
    let pack = pack_for(dir.path(), &hex);
    PackFixture { oid, pack }
}

/// Pack `commit` and everything it reaches from `dir`, by shelling out to
/// `git rev-list`/`git pack-objects` — the same bytes a real push
/// transmits, mirroring `odb-files`'s own test fixture.
fn pack_for(dir: &Path, commit: &str) -> Vec<u8> {
    let mut rev_list = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-list", "--objects", commit])
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn git rev-list");
    let pack_objects = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["pack-objects", "--stdout", "-q"])
        .stdin(rev_list.stdout.take().expect("rev-list stdout"))
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn git pack-objects");
    let output = pack_objects
        .wait_with_output()
        .expect("wait for pack-objects");
    assert!(rev_list.wait().expect("wait for rev-list").success());
    assert!(output.status.success());
    output.stdout
}
