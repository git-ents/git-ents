//! Shared fixtures for the WS9 maintenance tests: real git objects and
//! packs, built the same way `odb-files`' and `backend-conformance`'s own
//! fixtures build them.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test fixtures, not application code"
)]
#![allow(
    dead_code,
    reason = "shared by several test binaries, each using a different subset"
)]

use std::path::Path;
use std::process::{Command, Stdio};

use git_backend::{Expected, ObjectStore, PackStream, RefEdit, RefName, RefStore, TxOutcome};
use gix_hash::ObjectId;

/// Run `git` in `dir`, asserting success.
pub fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed in {dir:?}");
}

/// Pin a fresh repository's unborn `HEAD` to `refs/heads/main`, so tests
/// can name the default branch regardless of the host's
/// `init.defaultBranch`.
pub fn use_main_branch(dir: &Path) {
    git(dir, &["symbolic-ref", "HEAD", "refs/heads/main"]);
}

/// A pack of `revspec` and everything it reaches, via
/// `git rev-list | git pack-objects` — the same bytes a push transmits.
pub fn pack_for(dir: &Path, revspec: &str) -> Vec<u8> {
    let mut rev_list = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-list", "--objects", revspec])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let pack_objects = Command::new("git")
        .arg("-C")
        .arg(dir)
        // `--delta-base-offset`: emit OFS deltas (what a push negotiates),
        // which gitoxide's indexer resolves in-pack; the REF deltas
        // pack-objects emits by default assume a lookup no staged pack has.
        .args(["pack-objects", "--stdout", "-q", "--delta-base-offset"])
        .stdin(rev_list.stdout.take().unwrap())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let output = pack_objects.wait_with_output().unwrap();
    assert!(rev_list.wait().unwrap().success());
    assert!(output.status.success());
    output.stdout
}

/// A pack containing exactly the objects named by `oids` (hex), fed to
/// `git pack-objects` directly — for packing dangling blobs a rev walk
/// would never reach.
pub fn pack_of(dir: &Path, oids: &[&str]) -> Vec<u8> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["pack-objects", "--stdout", "-q"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write as _;
        let mut stdin = child.stdin.take().unwrap();
        for oid in oids {
            writeln!(stdin, "{oid}").unwrap();
        }
    }
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    output.stdout
}

/// Write `content` as a blob into `dir`'s object database, returning its
/// hex id.
pub fn hash_blob(dir: &Path, content: &str) -> String {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["hash-object", "-w", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write as _;
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(content.as_bytes()).unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// Parse a hex object id.
pub fn oid(hex: &str) -> ObjectId {
    ObjectId::from_hex(hex.as_bytes()).unwrap()
}

/// Stage `pack` into `store` and promote it immediately.
pub fn stage_and_promote(store: &dyn ObjectStore, pack: Vec<u8>) {
    let quarantine = store
        .stage_pack(PackStream::new(std::io::Cursor::new(pack)))
        .unwrap();
    store.promote(quarantine).unwrap();
}

/// Set `name` to `target` through a `RefStore` transaction (creating or
/// clobbering), asserting it applied.
pub fn set_ref(refs: &dyn RefStore, name: &str, target: ObjectId) {
    let outcome = refs
        .transaction(&[RefEdit {
            name: RefName::new(name),
            expected: Expected::Any,
            new: Some(target),
        }])
        .unwrap();
    assert!(matches!(outcome, TxOutcome::Applied));
}
