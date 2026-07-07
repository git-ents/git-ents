//! Shared fixtures for the native backend's unit tests: a throwaway bare
//! repository backed by the real local storage backends
//! (`refstore-files`/`odb-files`), a fixed single-repo resolver, and a real
//! SSH signer (a freshly generated throwaway key) — every test here signs
//! for real rather than faking an op record's attestation.
#![cfg(test)]
#![allow(clippy::unwrap_used, reason = "test fixture")]

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use git_member::members::Member;

use crate::attestation::{OpSigner, SshOpSigner};
use crate::native::{BackendResolver, RepoBackends};
use crate::{RepoId, Result};

/// A fresh bare repository on disk, deterministically branched at `main`.
pub fn bare_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let status = Command::new("git")
        .args(["init", "-q", "--bare", "-b", "main"])
        .arg(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    dir
}

/// Commit `content` as a new file in a scratch worktree cloned from `bare`,
/// pushing the result onto `bare`'s `main`, and return the new commit's id.
pub fn commit_onto(bare: &Path, file_name: &str, content: &str) -> gix_hash::ObjectId {
    let work = tempfile::tempdir().unwrap();
    run(work.path(), &["init", "-q", "-b", "main"]);
    run(work.path(), &["config", "user.email", "test@example.com"]);
    run(work.path(), &["config", "user.name", "test"]);
    std::fs::write(work.path().join(file_name), content).unwrap();
    run(work.path(), &["add", "-A"]);
    run(work.path(), &["commit", "-q", "-m", "test commit"]);
    let hex = String::from_utf8(
        Command::new("git")
            .arg("-C")
            .arg(work.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let hex = hex.trim();
    run(
        work.path(),
        &["push", bare.to_str().unwrap(), "main:refs/heads/main"],
    );
    gix_hash::ObjectId::from_hex(hex.as_bytes()).unwrap()
}

fn run(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .unwrap();
    assert!(status.success());
}

/// A [`BackendResolver`] over one fixed repository, ignoring the
/// [`RepoId`] every call names — every native-backend unit test only ever
/// needs one repo.
pub struct FixedResolver {
    /// The fixed repository's ref store.
    pub refs: Arc<dyn git_backend::RefStore>,
    /// The fixed repository's object store.
    pub objects: Arc<dyn git_backend::ObjectStore>,
    /// Members trusted to sign a push; empty leaves the bootstrap window
    /// open.
    pub authorized_members: Vec<Member>,
    /// The config `authorized_members`' roles are checked against.
    pub config: git_ents_core::config::Config,
}

impl FixedResolver {
    /// Open `refstore-files`/`odb-files` over `path`, with no members
    /// enrolled (bootstrap window open) unless overridden.
    pub fn open(path: &Path) -> Self {
        Self {
            refs: Arc::new(refstore_files::FilesRefStore::open(path).unwrap()),
            objects: Arc::new(odb_files::OdbFiles::open(path).unwrap()),
            authorized_members: Vec::new(),
            config: git_ents_core::config::Config::default(),
        }
    }
}

impl BackendResolver for FixedResolver {
    fn resolve(&self, _repo: &RepoId) -> Result<RepoBackends> {
        Ok(RepoBackends {
            refs: self.refs.clone(),
            objects: self.objects.clone(),
            authorized_members: self.authorized_members.clone(),
            config: self.config.clone(),
        })
    }
}

/// A real [`OpSigner`] backed by a freshly generated, passphrase-less
/// ed25519 key — real signing, not a stand-in, so op-record tests exercise
/// the same `ssh-keygen -Y sign` path production uses.
pub fn test_signer() -> (tempfile::TempDir, Arc<dyn OpSigner>) {
    let dir = tempfile::tempdir().unwrap();
    let key_path = dir.path().join("op_signing_key");
    let status = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&key_path)
        .status()
        .unwrap();
    assert!(status.success());
    (dir, Arc::new(SshOpSigner::new(key_path)))
}
