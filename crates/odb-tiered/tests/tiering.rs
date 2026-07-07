//! Exercises `OdbTiered::stage_pack`'s size-based split
//! (`crates/odb-tiered/src/lib.rs`): a pack containing both a small blob
//! (routed to the small tier) and a large blob (repacked and routed to the
//! underlying store) must come back byte-correct for both, in one
//! `stage_pack`/`promote` call.

#![allow(clippy::expect_used, reason = "test harness, not application code")]

use std::process::{Command, Stdio};

use git_backend::{ObjectStore as _, PackStream};
use gix_hash::{Kind as HashKind, ObjectId};
use odb_tiered::OdbTiered;
use odb_tiered::small_tier::memory::InMemorySmallTier;
use odb_tigris::OdbTigris;
use odb_tigris::registry::memory::InMemoryRegistry;
use odb_tigris::transport::fs::FsTransport;

fn blob_oid(data: &[u8]) -> ObjectId {
    let mut hasher = gix_hash::hasher(HashKind::Sha1);
    hasher.update(format!("blob {}\0", data.len()).as_bytes());
    hasher.update(data);
    hasher.try_finalize().expect("hash blob")
}

/// A real pack containing `commit` and everything it reaches, built the
/// same way `odb-files`' and `backend-conformance`'s own fixtures are.
fn pack_for(dir: &std::path::Path, commit: &str) -> Vec<u8> {
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
    let output = pack_objects.wait_with_output().expect("wait pack-objects");
    assert!(rev_list.wait().expect("wait rev-list").success());
    assert!(output.status.success());
    output.stdout
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .expect("run git");
    assert!(status.success());
}

#[test]
fn stages_small_and_large_objects_from_one_pack_correctly() {
    let small_content = b"tiny document content".to_vec(); // well under any sane threshold
    let large_content = vec![b'x'; 200_000]; // well over the default threshold

    let dir = tempfile::tempdir().expect("scratch repo dir");
    git(dir.path(), &["init", "-q"]);
    git(dir.path(), &["config", "user.email", "test@example.com"]);
    git(dir.path(), &["config", "user.name", "Test"]);
    std::fs::write(dir.path().join("small.txt"), &small_content).expect("write small file");
    std::fs::write(dir.path().join("large.bin"), &large_content).expect("write large file");
    git(dir.path(), &["add", "small.txt", "large.bin"]);
    git(dir.path(), &["commit", "-q", "-m", "tiering fixture"]);
    let commit_hex = String::from_utf8(
        Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("rev-parse")
            .stdout,
    )
    .expect("utf8")
    .trim()
    .to_owned();

    let pack_bytes = pack_for(dir.path(), &commit_hex);

    let bucket_dir = tempfile::tempdir().expect("bucket dir");
    let transport = FsTransport::open(bucket_dir.path().join("bucket")).expect("open transport");
    let underlying = OdbTigris::new(transport, InMemoryRegistry::new(), "tiering-repo");
    let store = OdbTiered::new(underlying, InMemorySmallTier::new(), "tiering-repo");

    let quarantine = store
        .stage_pack(PackStream::new(std::io::Cursor::new(pack_bytes)))
        .expect("stage_pack splits the incoming pack by size");
    store.promote(quarantine).expect("promote");

    let small_object = store
        .read(blob_oid(&small_content))
        .expect("read small object back");
    assert_eq!(small_object.data, small_content);

    let large_object = store
        .read(blob_oid(&large_content))
        .expect("read large object back");
    assert_eq!(large_object.data, large_content);
}
