//! Gated conformance for the WS5 Postgres implementations this crate adds:
//! [`odb_tigris::registry::PackRegistry`] (`pack_registry.rs`) and
//! [`odb_tiered::small_tier::SmallObjectTier`] (`small_tier.rs`), both over
//! `PostgresRefStore`. Reuses `tests/conformance.rs`'s own
//! docker-or-`GIT_ENTS_TEST_POSTGRES_URL` gating pattern (duplicated here
//! rather than shared, since Rust integration test binaries can't import
//! each other's private items) — see that file's module doc for the
//! priority order and the visible-skip rationale.
//!
//! The bucket side of `OdbTigris` uses `FsTransport` here, not a real S3
//! bucket: this test's job is exercising the two Postgres-backed traits,
//! not re-verifying `odb-tigris`'s own transport-agnostic conformance
//! (already covered, over `FsTransport` + an in-memory registry, in
//! `crates/odb-tigris/tests/conformance.rs`).

#![allow(clippy::expect_used, reason = "test harness, not application code")]

use std::process::{Command, Stdio};
use std::time::Duration;

use backend_conformance::NoopCollector;
use git_backend::{Object, ObjectStore, PackStream, QuarantineId, Result};
use gix_hash::ObjectId;
use odb_tiered::OdbTiered;
use odb_tigris::OdbTigris;
use odb_tigris::transport::fs::FsTransport;
use refstore_postgres::PostgresRefStore;

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
            "refstore-postgres odb_ws5_conformance: docker run failed: {}",
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

type Underlying = OdbTigris<FsTransport, PostgresRefStore>;

/// Bundles a store composed over two `PostgresRefStore` connections (one
/// playing `PackRegistry`, one playing `SmallObjectTier`) with the tempdir
/// its `FsTransport` bucket root lives under.
struct WithPostgres {
    store: OdbTiered<Underlying, PostgresRefStore>,
    _dir: tempfile::TempDir,
}

impl WithPostgres {
    fn new(url: &str) -> Self {
        let repo_id = format!("ws5-conformance-{}", uuid::Uuid::new_v4());
        let dir = tempfile::tempdir().expect("tempdir");
        let transport = FsTransport::open(dir.path().join("bucket")).expect("open transport");
        let registry = PostgresRefStore::connect(url, repo_id.clone()).expect("connect registry");
        let underlying = OdbTigris::new(transport, registry, repo_id.clone());
        let small_tier =
            PostgresRefStore::connect(url, repo_id.clone()).expect("connect small tier");
        let store = OdbTiered::new(underlying, small_tier, repo_id);
        Self { store, _dir: dir }
    }
}

impl ObjectStore for WithPostgres {
    fn read(&self, id: ObjectId) -> Result<Object> {
        self.store.read(id)
    }

    fn contains(&self, id: ObjectId) -> Result<bool> {
        self.store.contains(id)
    }

    fn stage_pack(&self, pack: PackStream) -> Result<QuarantineId> {
        self.store.stage_pack(pack)
    }

    fn promote(&self, q: QuarantineId) -> Result<()> {
        self.store.promote(q)
    }
}

#[test]
fn conforms_to_object_store_properties_over_postgres() {
    let pg = require_postgres!("conforms_to_object_store_properties_over_postgres");
    let url = pg.url().to_owned();
    backend_conformance::object_store_properties(|| WithPostgres::new(&url), &NoopCollector);
}

#[test]
fn reachability_artifact_registry_round_trips_over_postgres() {
    use odb_tigris::registry::{ArtifactKind, ArtifactRecord, PackRegistry};

    let pg = require_postgres!("reachability_artifact_registry_round_trips_over_postgres");
    let repo_id = format!("ws6-artifact-{}", uuid::Uuid::new_v4());
    let registry = PostgresRefStore::connect(pg.url(), repo_id.clone()).expect("connect registry");

    assert!(
        registry
            .get_artifact(&repo_id, ArtifactKind::CommitGraph)
            .expect("get_artifact")
            .is_none()
    );

    registry
        .record_artifact(ArtifactRecord {
            repo_id: repo_id.clone(),
            kind: ArtifactKind::CommitGraph,
            key: "some/key.bin".to_owned(),
        })
        .expect("record_artifact");

    let record = registry
        .get_artifact(&repo_id, ArtifactKind::CommitGraph)
        .expect("get_artifact")
        .expect("artifact was just recorded");
    assert_eq!(record.key, "some/key.bin");

    // Recording again for the same `(repo_id, kind)` replaces it, rather
    // than accumulating a second row.
    registry
        .record_artifact(ArtifactRecord {
            repo_id: repo_id.clone(),
            kind: ArtifactKind::CommitGraph,
            key: "new/key.bin".to_owned(),
        })
        .expect("re-record_artifact");
    let record = registry
        .get_artifact(&repo_id, ArtifactKind::CommitGraph)
        .expect("get_artifact")
        .expect("artifact still present");
    assert_eq!(record.key, "new/key.bin");

    registry
        .delete_artifact(&repo_id, ArtifactKind::CommitGraph)
        .expect("delete_artifact");
    assert!(
        registry
            .get_artifact(&repo_id, ArtifactKind::CommitGraph)
            .expect("get_artifact")
            .is_none()
    );
}
