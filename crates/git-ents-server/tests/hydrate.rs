#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    reason = "integration test binary"
)]

//! End-to-end coverage for the WS0 hydration backend (`docs/scale-out.adoc`'s
//! "WS0 Interim hydration backend" section).
//!
//! Two real `git push` invocations run over HTTP against a server
//! configured with `GIT_ENTS_HYDRATE_POSTGRES_URL` and
//! `GIT_ENTS_HYDRATE_BLOB_ROOT`, so every request goes through
//! `git_hydrate`'s read/write paths rather than direct disk. The test then
//! replays the corpus the write path logged, using
//! `backend_conformance::replay_corpus`, against fresh
//! `refstore-files`/`odb-files` backends, and asserts the replayed backend
//! ends up with identical content refs and an identical reachable object
//! set to the original Postgres/Tigris-backed repository: the conformance
//! seed corpus `docs/scale-out.adoc` asks WS0 to produce.
//!
//! Gated on a reachable Postgres (`GIT_ENTS_TEST_POSTGRES_URL`, or a
//! throwaway docker container), matching `refstore-postgres`'s own tests.
//! See that crate's `tests/conformance.rs` module doc for the priority
//! order and the visible-skip rationale.

use std::collections::BTreeSet;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use git_backend::{RefName, RefStore as _};
use gix_hash::ObjectId;
use odb_tigris::OdbTigris;
use odb_tigris::transport::fs::FsTransport;
use refstore_postgres::PostgresRefStore;

const BIN: &str = env!("CARGO_BIN_EXE_git-ents-server");

enum TestPostgres {
    External(String),
    Docker { container_id: String, url: String },
}

impl TestPostgres {
    fn url(&self) -> &str {
        match self {
            Self::External(url) | Self::Docker { url, .. } => url,
        }
    }
}

impl Drop for TestPostgres {
    fn drop(&mut self) {
        if let Self::Docker { container_id, .. } = self {
            let _ignored = Command::new("docker")
                .args(["rm", "-f", container_id])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

fn test_postgres() -> Option<TestPostgres> {
    if let Ok(url) = std::env::var("GIT_ENTS_TEST_POSTGRES_URL") {
        return Some(TestPostgres::External(url));
    }
    if !docker_available() {
        return None;
    }
    start_docker_postgres()
}

fn docker_available() -> bool {
    Command::new("docker")
        .arg("version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn start_docker_postgres() -> Option<TestPostgres> {
    let output = Command::new("docker")
        .args([
            "run",
            "-d",
            "--rm",
            "-e",
            "POSTGRES_PASSWORD=postgres",
            "-p",
            "127.0.0.1::5432",
            "postgres:16-alpine",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        eprintln!(
            "git-ents-server hydrate test: docker run failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return None;
    }
    let container_id = String::from_utf8_lossy(&output.stdout).trim().to_owned();

    for _ in 0..120 {
        let ready = Command::new("docker")
            .args(["exec", &container_id, "pg_isready", "-U", "postgres"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if ready {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }

    let port_output = Command::new("docker")
        .args(["port", &container_id, "5432"])
        .output()
        .ok()?;
    let mapping = String::from_utf8_lossy(&port_output.stdout);
    let port = mapping
        .lines()
        .next()?
        .rsplit(':')
        .next()?
        .trim()
        .to_owned();

    // `pg_isready` above checks the container's internal socket, which can
    // report ready slightly before the published TCP port is actually
    // reachable from the host. Confirm a real connection before handing the
    // URL to callers.
    if !wait_for_tcp(&format!("127.0.0.1:{port}")) {
        eprintln!("git-ents-server hydrate test: postgres port never became reachable");
        return None;
    }

    Some(TestPostgres::Docker {
        url: format!("host=127.0.0.1 port={port} user=postgres password=postgres dbname=postgres"),
        container_id,
    })
}

fn wait_for_tcp(addr: &str) -> bool {
    for _ in 0..40 {
        if std::net::TcpStream::connect(addr).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    false
}

macro_rules! require_postgres {
    ($name:literal) => {
        match test_postgres() {
            Some(pg) => pg,
            None => {
                eprintln!(concat!(
                    "skipping ",
                    $name,
                    ": set GIT_ENTS_TEST_POSTGRES_URL, or make docker available"
                ));
                return;
            }
        }
    };
}

// @relation(protocol.git, storage.bare, role=Verifies)
#[test]
fn pushes_through_hydration_replay_identically_against_the_files_backends() {
    let pg =
        require_postgres!("pushes_through_hydration_replay_identically_against_the_files_backends");
    let repo_id = format!("hydrate-{}.git", uuid::Uuid::new_v4());

    let scratch = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let blob_root = tempfile::tempdir().unwrap();
    let hooks = tempfile::tempdir().unwrap();

    let hook = hooks.path().join("pre-receive");
    std::fs::write(&hook, format!("#!/bin/sh\nexec \"{BIN}\" pre-receive\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let signing_key = keygen(scratch.path(), "op-signer");

    let port = free_port();
    let mut child = Command::new(BIN)
        .args(["--port", &port.to_string()])
        .arg("--data-dir")
        .arg(data.path())
        .arg("--hooks-dir")
        .arg(hooks.path())
        .arg("--web-signing-key")
        .arg(&signing_key)
        .arg("--checks-queue")
        .arg(scratch.path().join("checks-queue"))
        .env("GIT_ENTS_HYDRATE_POSTGRES_URL", pg.url())
        .env("GIT_ENTS_HYDRATE_BLOB_ROOT", blob_root.path())
        .spawn()
        .unwrap();
    wait_for_port(port);

    let url = format!("http://127.0.0.1:{port}/{repo_id}");
    let work = scratch.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    run(&work, "git", &["init", "-q", "-b", "main"]);
    std::fs::write(work.join("file.txt"), "one\n").unwrap();
    run(&work, "git", &["add", "."]);
    run(
        &work,
        "git",
        &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "first"],
    );
    run(&work, "git", &["push", "-q", &url, "main"]);

    // A second push, so the corpus carries more than one entry and the
    // second's pack excludes the first's already-known objects.
    std::fs::write(work.join("file.txt"), "two\n").unwrap();
    run(&work, "git", &["add", "."]);
    run(
        &work,
        "git",
        &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "second"],
    );
    run(&work, "git", &["push", "-q", &url, "main"]);

    child.kill().unwrap();
    let _wait = child.wait();

    // The source of truth: Postgres refs, Tigris (here, `FsTransport`)
    // objects, and the corpus this repository's pushes logged.
    let source_refs = PostgresRefStore::connect(pg.url(), repo_id.clone()).unwrap();
    let source_registry = PostgresRefStore::connect(pg.url(), repo_id.clone()).unwrap();
    let source_transport = FsTransport::open(blob_root.path()).unwrap();
    let source_objects = OdbTigris::new(source_transport, source_registry, repo_id.clone());

    let entries = source_refs.corpus_log().unwrap();
    assert_eq!(
        entries.len(),
        2,
        "both pushes should have logged a corpus entry"
    );

    let main = RefName::new("refs/heads/main");
    let source_main = source_refs.get(&main).unwrap();
    assert!(
        source_main.is_some(),
        "the pushed branch must exist in Postgres"
    );

    // Replay the corpus against fresh `refstore-files`/`odb-files` — the
    // conformance seed corpus (`docs/scale-out.adoc`, WS2) this backend
    // feeds.
    let target_dir = tempfile::tempdir().unwrap();
    run(target_dir.path(), "git", &["init", "-q", "--bare"]);
    let target_refs = refstore_files::FilesRefStore::open(target_dir.path()).unwrap();
    let target_objects = odb_files::OdbFiles::open(target_dir.path()).unwrap();
    backend_conformance::replay_corpus(&entries, &target_refs, &target_objects).unwrap();

    let target_main = target_refs.get(&main).unwrap();
    assert_eq!(
        target_main, source_main,
        "replaying the corpus must reproduce the same final ref"
    );

    let source_reachable = reachable_from_heads(&source_refs, &source_objects);
    let target_reachable = reachable_from_heads(&target_refs, &target_objects);
    assert_eq!(
        source_reachable, target_reachable,
        "replaying the corpus must reproduce the same reachable object set"
    );
}

/// The set of objects reachable from every `refs/heads/*` tip — deliberately
/// narrower than `backend_conformance::reachable_object_set` (which walks
/// every ref, including `refs/meta/ops/log`): the corpus intentionally does
/// not carry the op-record chain (it re-signs on every replay, so it never
/// hash-matches — see `git_protocol::corpus`'s module doc), so comparing the
/// content refs' reachable closure is the correct scope for this assertion.
fn reachable_from_heads(
    refs: &dyn git_backend::RefStore,
    objects: &dyn git_backend::ObjectStore,
) -> BTreeSet<ObjectId> {
    let roots: Vec<ObjectId> = refs
        .iter_prefix(&RefName::new("refs/heads/"))
        .unwrap()
        .map(|entry| entry.unwrap().1)
        .collect();
    let source = gix_reachability::walk::StoreSource::new(objects);
    gix_reachability::walk::reachable(roots, &source, |_id| false, false).unwrap()
}

fn keygen(base: &Path, name: &str) -> PathBuf {
    let key = base.join(name);
    let status = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", name, "-f"])
        .arg(&key)
        .status()
        .unwrap();
    assert!(status.success(), "ssh-keygen failed");
    key
}

fn free_port() -> u16 {
    let probe = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
    port
}

fn wait_for_port(port: u16) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match TcpStream::connect(format!("127.0.0.1:{port}")) {
            Ok(_) => return,
            Err(_) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("server never accepted connections: {error}"),
        }
    }
}

fn run(dir: &Path, program: &str, args: &[&str]) {
    let output = Command::new(program)
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@e")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@e")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{program} {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
