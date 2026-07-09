//! The dispatcher's claim/requeue/complete SQL against a real Postgres,
//! exercised through the [`effect_dispatcher::EffectQueue`] impl for
//! [`refstore_postgres::PostgresRefStore`]. Gated on a reachable Postgres
//! exactly like `refstore-postgres`' own suites (whose harness this
//! duplicates, as `odb_ws5_conformance` already does):
//!
//! 1. `GIT_ENTS_TEST_POSTGRES_URL`, if set — an already-running Postgres.
//! 2. A throwaway `docker run` Postgres container, if docker is available.
//! 3. Otherwise: a visible skip.
//!
//! One caveat the SQL makes unavoidable: `dispatcher_*` queries span every
//! `repo_id` by design, so against a *shared* external database
//! (`GIT_ENTS_TEST_POSTGRES_URL`) this test can claim rows other suites
//! enqueued. The docker path — one container per test — is fully isolated;
//! assertions below filter to this test's own repo ids rather than assert
//! global counts, so an externally shared database perturbs nothing here.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test harness and assertions, not application code"
)]

use std::process::{Command, Stdio};
use std::time::Duration;

use effect_dispatcher::EffectQueue as _;
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
            "effect-dispatcher postgres_queue: docker run failed: {}",
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
        eprintln!("effect-dispatcher postgres_queue: postgres port never became reachable");
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

#[test]
fn dispatcher_sql_claims_across_repos_requeues_stale_and_completes() {
    let Some(pg) = test_postgres() else {
        eprintln!(
            "skipping dispatcher_sql_claims_across_repos_requeues_stale_and_completes: \
             set GIT_ENTS_TEST_POSTGRES_URL, or make docker available"
        );
        return;
    };
    let repo_a = format!("dispatch-a-{}", uuid::Uuid::new_v4());
    let repo_b = format!("dispatch-b-{}", uuid::Uuid::new_v4());
    let store_a = PostgresRefStore::connect(pg.url(), repo_a.clone()).expect("connect a");
    let store_b = PostgresRefStore::connect(pg.url(), repo_b.clone()).expect("connect b");
    let a_id: i64 = store_a
        .enqueue_effect("payload-a")
        .expect("enqueue a")
        .into();
    let b_id: i64 = store_b
        .enqueue_effect("payload-b")
        .expect("enqueue b")
        .into();
    let mine = |id: i64, repo: &str| repo == repo_a && id == a_id || repo == repo_b && id == b_id;

    // The dispatcher claim spans repos: one query sees both stores' rows,
    // each attributed to its repo.
    let claimed = store_a.claim("dispatcher-1", 100, &[]).expect("claim");
    let claimed: Vec<_> = claimed
        .into_iter()
        .filter(|job| mine(job.id, &job.repo))
        .collect();
    assert_eq!(claimed.len(), 2);
    assert!(
        claimed
            .iter()
            .any(|job| job.id == a_id && job.repo == repo_a && job.payload == "payload-a")
    );
    assert!(
        claimed
            .iter()
            .any(|job| job.id == b_id && job.repo == repo_b && job.payload == "payload-b")
    );

    // A claimed row is not claimable again while its claim is fresh...
    let reclaimed = store_a
        .claim("dispatcher-2", 100, &[])
        .expect("claim while claimed");
    assert!(reclaimed.iter().all(|job| !mine(job.id, &job.repo)));

    // ...but a zero timeout makes every claim stale: both rows come back.
    let requeued = store_a
        .requeue_stale(Duration::ZERO)
        .expect("requeue stale");
    assert!(requeued >= 2);

    // The per-repo exclusion (a repo at its fairness cap) skips that
    // repo's rows and still claims the rest.
    let claimed = store_a
        .claim("dispatcher-3", 100, std::slice::from_ref(&repo_b))
        .expect("claim excluding b");
    let ids: Vec<i64> = claimed
        .iter()
        .filter(|job| mine(job.id, &job.repo))
        .map(|job| job.id)
        .collect();
    assert_eq!(ids, vec![a_id]);

    // Done is terminal: a completed row never comes back, even through a
    // zero-timeout requeue.
    store_a.complete(a_id).expect("complete a");
    let _requeued = store_a
        .requeue_stale(Duration::ZERO)
        .expect("requeue stale again");
    let claimed = store_a
        .claim("dispatcher-4", 100, &[])
        .expect("claim after complete");
    let ids: Vec<i64> = claimed
        .iter()
        .filter(|job| mine(job.id, &job.repo))
        .map(|job| job.id)
        .collect();
    assert_eq!(ids, vec![b_id]);
    store_b.complete(b_id).expect("complete b");
}
