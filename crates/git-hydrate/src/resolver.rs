//! [`PostgresResolver`]: a [`git_protocol::native::BackendResolver`] over
//! the durable stores — Postgres for refs (and, doubling as the pack
//! registry, for `odb-tigris`'s bookkeeping), the configured blob store for
//! packs. Feeding this resolver to
//! [`git_protocol::native::NativeBackend`] is what makes
//! [`crate::pre_receive::run`] the "IngestPack via receive-pack against a
//! scratch repo with Postgres as the commit point" backend
//! `docs/scale-out.adoc`'s "Protocol traits" section names.

use std::path::PathBuf;
use std::sync::Arc;

use git_backend::ObjectStore;
use git_protocol::native::{BackendResolver, RepoBackends};
use git_protocol::types::RepoId;
use odb_tigris::OdbTigris;
use odb_tigris::transport::fs::FsTransport;
use odb_tigris::transport::s3::S3Transport;
use refstore_postgres::PostgresRefStore;

use crate::config::{BlobStore, HydrateConfig};

/// Resolves a [`RepoId`] to `refstore-postgres`/`odb-tigris` backends, and
/// to the repository's currently enrolled members/config — read from the
/// local hydrated disk cache at `repo_path`, exactly as
/// `git_ents_server::native_git::DiskResolver` reads them for the WS3
/// native path, and as `git-signed-push`'s own `pre-receive` verifier
/// always has: a fresh disk read per call, which is close enough to
/// Postgres truth by the time a push reaches this resolver (`packed-refs`
/// was just regenerated from Postgres on the preceding `info/refs`).
pub struct PostgresResolver {
    config: HydrateConfig,
    repo_path: PathBuf,
}

impl PostgresResolver {
    /// Resolve repositories through `config`'s durable stores, reading
    /// members/config from the local hydrated cache at `repo_path`.
    #[must_use]
    pub fn new(config: HydrateConfig, repo_path: impl Into<PathBuf>) -> Self {
        Self {
            config,
            repo_path: repo_path.into(),
        }
    }
}

impl BackendResolver for PostgresResolver {
    fn resolve(&self, repo: &RepoId) -> git_protocol::Result<RepoBackends> {
        let refs = PostgresRefStore::connect(&self.config.postgres_conninfo, repo.as_str())?;
        let registry = PostgresRefStore::connect(&self.config.postgres_conninfo, repo.as_str())?;
        let objects: Arc<dyn ObjectStore> = match &self.config.blob {
            BlobStore::Fs(root) => {
                let transport = FsTransport::open(root)?;
                Arc::new(OdbTigris::new(
                    transport,
                    registry,
                    repo.as_str().to_owned(),
                ))
            }
            BlobStore::S3(s3_config) => {
                let transport = S3Transport::connect(s3_config)?;
                Arc::new(OdbTigris::new(
                    transport,
                    registry,
                    repo.as_str().to_owned(),
                ))
            }
        };

        let members = git_member::members::load_all(&self.repo_path)
            .map_err(|error| git_protocol::Error::UnknownRepo(error.to_string()))?;
        let revoked = git_member::revocations::fingerprints(&self.repo_path)
            .map_err(|error| git_protocol::Error::UnknownRepo(error.to_string()))?;
        let config = git_ents_core::config::load(&self.repo_path)
            .map_err(|error| git_protocol::Error::UnknownRepo(error.to_string()))?;

        Ok(RepoBackends {
            refs: Arc::new(refs),
            objects,
            authorized_members: git_member::members::without_revoked(members, &revoked),
            config,
            // No reachability artifacts wired for this resolver yet (same
            // gap `DiskResolver` documents): negotiation/ingest degrade to
            // the plain walk, never a wrong answer.
            reachability: git_reachability::ArtifactBundle::empty(),
        })
    }
}
