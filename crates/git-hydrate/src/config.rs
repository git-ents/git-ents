//! [`HydrateConfig`]: how to reach the durable stores hydration reads from
//! and writes through. Present (`Some`) enables hydration mode; absent
//! keeps a deployment on the current direct-disk behavior, per
//! `docs/scale-out.adoc`'s thesis that cloud deployment is additive
//! configuration, not a different code path.

use std::path::PathBuf;

use odb_tigris::transport::s3::S3Config;

/// Which [`odb_tigris::transport::BlobTransport`] hydration reads packs
/// from and writes them to.
#[derive(Debug, Clone)]
pub enum BlobStore {
    /// A local directory, standing in for the bucket — used by tests and
    /// small/self-hosted deployments that don't need S3.
    Fs(PathBuf),
    /// A real S3-compatible bucket (Tigris in production).
    S3(S3Config),
}

/// Everything hydration needs to reach the durable stores: a Postgres
/// connection string (`refstore-postgres`'s ref store, reflog, pack
/// registry, and op-replay corpus log) and a blob store (`odb-tigris`'s
/// packs).
#[derive(Debug, Clone)]
pub struct HydrateConfig {
    /// Libpq connection string for the Postgres ref store / pack registry /
    /// corpus log.
    pub postgres_conninfo: String,
    /// Where packs live.
    pub blob: BlobStore,
}

impl HydrateConfig {
    /// Build a config over a local directory blob store — the common case
    /// for tests and small deployments.
    #[must_use]
    pub fn with_fs_blob(postgres_conninfo: impl Into<String>, root: impl Into<PathBuf>) -> Self {
        Self {
            postgres_conninfo: postgres_conninfo.into(),
            blob: BlobStore::Fs(root.into()),
        }
    }

    /// Read a config from the environment, or `None` if hydration is not
    /// configured (the caller should then keep the current direct-disk
    /// behavior). Recognizes:
    ///
    /// - `GIT_ENTS_HYDRATE_POSTGRES_URL` (required to enable hydration).
    /// - `GIT_ENTS_HYDRATE_BLOB_ROOT` — a local directory blob store, or
    /// - `GIT_ENTS_HYDRATE_S3_BUCKET`/`_REGION`/`_ENDPOINT`/
    ///   `_ACCESS_KEY_ID`/`_SECRET_ACCESS_KEY`/`_ALLOW_HTTP` — an
    ///   S3-compatible bucket. The `Fs` root wins if both are set.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        let postgres_conninfo = env_var("GIT_ENTS_HYDRATE_POSTGRES_URL")?;
        if let Some(root) = env_var("GIT_ENTS_HYDRATE_BLOB_ROOT") {
            return Some(Self {
                postgres_conninfo,
                blob: BlobStore::Fs(PathBuf::from(root)),
            });
        }
        let bucket = env_var("GIT_ENTS_HYDRATE_S3_BUCKET")?;
        let region = env_var("GIT_ENTS_HYDRATE_S3_REGION").unwrap_or_else(|| "auto".to_owned());
        let endpoint = env_var("GIT_ENTS_HYDRATE_S3_ENDPOINT")?;
        let access_key_id = env_var("GIT_ENTS_HYDRATE_S3_ACCESS_KEY_ID")?;
        let secret_access_key = env_var("GIT_ENTS_HYDRATE_S3_SECRET_ACCESS_KEY")?;
        let allow_http = env_var("GIT_ENTS_HYDRATE_S3_ALLOW_HTTP").is_some_and(|v| v == "1");
        Some(Self {
            postgres_conninfo,
            blob: BlobStore::S3(S3Config {
                bucket,
                region,
                endpoint,
                access_key_id,
                secret_access_key,
                allow_http,
            }),
        })
    }
}

fn env_var(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|value| !value.is_empty())
}
