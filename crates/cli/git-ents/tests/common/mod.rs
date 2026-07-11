//! Shared test fixtures for `git-ents`'s integration suite: a real,
//! on-disk repository plus a deterministic signing key — the counterpart
//! to `ents-testutil`'s in-memory fixtures, needed here because this
//! crate's composition roots ([`git_ents::root::LocalRoot`],
//! [`git_ents::root::HostedRoot`]) wire a real `LooseRefStore` and a real
//! odb, not the in-memory `MemRefStore`/`ObjectStore` pair every library
//! crate's own tests use.

#![allow(dead_code, reason = "not every test file uses every helper")]
#![allow(clippy::expect_used, reason = "integration test")]

use std::path::{Path, PathBuf};

use ssh_key::private::{Ed25519Keypair, KeypairData};
use ssh_key::{LineEnding, PrivateKey};
use tempfile::TempDir;

/// A real, empty git repository plus a deterministic signing key, ready
/// for [`git_ents::root::LocalRoot::open`] or [`git_ents::root::HostedRoot::open`].
pub struct Fixture {
    pub dir: TempDir,
    pub key_path: PathBuf,
}

impl Fixture {
    /// Initialize a fresh, non-bare repository with a deterministic
    /// ed25519 signing key at `<repo>/../id_ed25519`, seeded by `seed`.
    pub fn new(seed: u8) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        gix::init(dir.path()).expect("init");
        let key_path = dir.path().join(".id_ed25519");
        write_key(&key_path, seed);
        Self { dir, key_path }
    }

    /// Initialize a fresh *bare* repository (the single-node hosted root's
    /// shape) with a deterministic signing key.
    pub fn new_bare(seed: u8) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        gix::init_bare(dir.path()).expect("init bare");
        let key_path = dir.path().join(".id_ed25519");
        write_key(&key_path, seed);
        Self { dir, key_path }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }
}

/// Write a deterministic key inside `dir` (as `.id_ed25519`) and return its
/// path — for tests that need a key living alongside a specific working
/// directory (a clone) rather than a [`Fixture`]'s own repo directory.
pub fn write_key_in(dir: &Path, seed: u8) -> PathBuf {
    let path = dir.join(".id_ed25519");
    write_key(&path, seed);
    path
}

/// Write a deterministic, unencrypted ed25519 key at `path` — the fixture
/// counterpart to `ents_testutil::Keypair::from_seed`, but a real file a
/// [`git_ents::sign::Signer`] can load.
pub fn write_key(path: &Path, seed: u8) {
    let pair = Ed25519Keypair::from_seed(&[seed; 32]);
    let key = PrivateKey::new(KeypairData::from(pair), "git-ents-test").expect("well-formed");
    key.write_openssh_file(path, LineEnding::LF)
        .expect("write key");
}

/// The path to the built `git-ents` binary under test — `cargo test`
/// exposes this via `CARGO_BIN_EXE_<name>`.
pub fn bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_git-ents"))
}
