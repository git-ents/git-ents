//! Conformance and targeted tests for `refstore-postgres`
//! (`docs/scale-out.adoc`, WS4 / WS2), gated on a reachable Postgres:
//!
//! 1. `GIT_ENTS_TEST_POSTGRES_URL`, if set — an already-running Postgres.
//! 2. A throwaway `docker run` Postgres container, if docker is available.
//! 3. Otherwise: print a message and return — a visible skip (matching
//!    `git-effect`'s own `docker_backend_runs_a_trivial_effect` gate), not a
//!    silently-green test.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test harness and assertions, not application code"
)]

use std::process::{Command, Stdio};
use std::time::Duration;

use backend_conformance::WithScratchRepo;
use git_backend::{Expected, RefEdit, RefName, RefStore as _};
use refstore_postgres::PostgresRefStore;

/// A reachable test Postgres: either an externally supplied instance or a
/// throwaway docker container this harness starts and stops.
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

/// Obtain a test Postgres per the priority order in the module doc, or
/// `None` if neither an external URL nor docker is available.
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
            "refstore-postgres tests: docker run failed: {}",
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

    Some(TestPostgres::Docker {
        url: format!("host=127.0.0.1 port={port} user=postgres password=postgres dbname=postgres"),
        container_id,
    })
}

/// Get a [`TestPostgres`], or print a skip message and return from the
/// calling test — a visible skip rather than a silently-green pass.
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

#[test]
fn conforms_to_ref_store_properties() {
    let pg = require_postgres!("conforms_to_ref_store_properties");
    let url = pg.url().to_owned();
    backend_conformance::ref_store_properties(move || {
        let repo_id = format!("conformance-{}", uuid::Uuid::new_v4());
        let url = url.clone();
        WithScratchRepo::new(move |_path| PostgresRefStore::connect(&url, repo_id))
    });
}

#[test]
fn notify_hint_fires_on_a_transaction_commit() {
    let pg = require_postgres!("notify_hint_fires_on_a_transaction_commit");
    let repo_id = format!("notify-{}", uuid::Uuid::new_v4());
    let store = PostgresRefStore::connect(pg.url(), repo_id).expect("connect");

    let watcher = store.watch(&RefName::new("refs/")).expect("watch");
    let oid = backend_conformance::distinct_oids(1)
        .into_iter()
        .next()
        .expect("one oid");
    store
        .transaction(&[RefEdit {
            name: RefName::new("refs/heads/watched"),
            expected: Expected::MustNotExist,
            new: Some(oid),
        }])
        .expect("transaction");

    assert!(
        watcher.recv_timeout(Duration::from_secs(10)).is_some(),
        "expected a NOTIFY-driven wakeup hint after a committed transaction"
    );
}

#[test]
fn queue_table_survives_a_dropped_connection() {
    let pg = require_postgres!("queue_table_survives_a_dropped_connection");
    let repo_id = format!("queue-{}", uuid::Uuid::new_v4());

    let id = {
        let store = PostgresRefStore::connect(pg.url(), repo_id.clone()).expect("connect");
        store.enqueue_effect("payload-a").expect("enqueue")
    };
    // `store` (and its one connection) is dropped here; the row must
    // survive in Postgres regardless, per the queue table's at-least-once
    // contract (`docs/scale-out.adoc`, "RefStore").

    let store = PostgresRefStore::connect(pg.url(), repo_id).expect("reconnect");
    let claimed = store.claim_effects("worker-1", 10).expect("claim");
    assert_eq!(claimed.len(), 1);
    let claimed_one = claimed.first().expect("one claimed row");
    assert_eq!(claimed_one.id, id);
    assert_eq!(claimed_one.payload, "payload-a");
    store.complete_effect(id).expect("complete");
}

#[test]
fn maintenance_advisory_lock_serializes_per_repo_across_sessions() {
    let pg = require_postgres!("maintenance_advisory_lock_serializes_per_repo_across_sessions");
    let repo_id = format!("maintenance-{}", uuid::Uuid::new_v4());

    // Two sessions (two connections) contending for one repo's
    // maintenance lock (`docs/scale-out.adoc`, WS9: "Per-repo background
    // effects serialized by advisory lock"): the second skips while the
    // first holds it.
    let holder = PostgresRefStore::connect(pg.url(), repo_id.clone()).expect("connect holder");
    let contender =
        PostgresRefStore::connect(pg.url(), repo_id.clone()).expect("connect contender");
    assert!(holder.try_maintenance_lock().expect("holder acquires"));
    assert!(
        !contender.try_maintenance_lock().expect("contender tries"),
        "a concurrent run must skip while the repo's lock is held"
    );

    // The lock is keyed by repo: a different repository is unaffected.
    let other_repo = format!("maintenance-{}", uuid::Uuid::new_v4());
    let other = PostgresRefStore::connect(pg.url(), other_repo).expect("connect other repo");
    assert!(other.try_maintenance_lock().expect("other repo acquires"));

    // Release: the contender proceeds.
    assert!(holder.unlock_maintenance().expect("holder releases"));
    assert!(
        contender
            .try_maintenance_lock()
            .expect("contender acquires after release")
    );
}
