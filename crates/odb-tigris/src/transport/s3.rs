//! [`S3Transport`]: the real [`BlobTransport`], over any S3-compatible
//! bucket (Tigris included) via the `object_store` crate's `aws` feature
//! (`docs/scale-out.adoc`, WS5). Dependency policy: `object_store` only,
//! no `aws-sdk-*`, no `reqwest`/`hyper` pulled in directly.
//!
//! `object_store`'s trait is `async`; [`BlobTransport`] is not, so this type
//! owns one dedicated [`tokio::runtime::Runtime`] and `block_on`s every
//! call, mirroring `refstore-postgres::PostgresRefStore`'s own bridge from a
//! sync trait to an async client.

use std::ops::Range;

use git_backend::Result;
use object_store::aws::AmazonS3Builder;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStoreExt as _, PutPayload};

use super::{BlobTransport, transport_err};

/// Connection parameters for an S3-compatible bucket. Kept minimal and
/// explicit rather than reading environment variables implicitly, so a
/// caller embedding this crate controls exactly what credentials and
/// endpoint it targets.
#[derive(Debug, Clone)]
pub struct S3Config {
    /// The bucket name.
    pub bucket: String,
    /// The region to sign requests for (Tigris and most S3-compatibles
    /// accept an arbitrary non-empty value here).
    pub region: String,
    /// The S3-compatible endpoint URL (e.g. `https://fly.storage.tigris.dev`).
    pub endpoint: String,
    /// Access key id.
    pub access_key_id: String,
    /// Secret access key.
    pub secret_access_key: String,
    /// Whether to allow plain HTTP (only ever `true` in tests against a
    /// local S3-compatible stand-in).
    pub allow_http: bool,
}

/// A [`BlobTransport`] over an S3-compatible bucket.
pub struct S3Transport {
    runtime: tokio::runtime::Runtime,
    store: object_store::aws::AmazonS3,
}

impl S3Transport {
    /// Connect to the bucket described by `config`.
    ///
    /// # Errors
    ///
    /// Returns an error if the dedicated runtime cannot be created or the
    /// `object_store` client fails to build (e.g. malformed config).
    pub fn connect(config: &S3Config) -> Result<Self> {
        let runtime = tokio::runtime::Runtime::new()
            .map_err(|error| transport_err("creating S3 transport runtime", error))?;
        let store = AmazonS3Builder::new()
            .with_bucket_name(&config.bucket)
            .with_region(&config.region)
            .with_endpoint(&config.endpoint)
            .with_access_key_id(&config.access_key_id)
            .with_secret_access_key(&config.secret_access_key)
            .with_allow_http(config.allow_http)
            .build()
            .map_err(|error| transport_err("building S3 client", error))?;
        Ok(Self { runtime, store })
    }
}

impl BlobTransport for S3Transport {
    fn put(&self, key: &str, bytes: Vec<u8>) -> Result<()> {
        let path = ObjectPath::from(key);
        self.runtime
            .block_on(async { self.store.put(&path, PutPayload::from(bytes)).await })
            .map_err(|error| transport_err(&format!("put {key}"), error))?;
        Ok(())
    }

    fn get(&self, key: &str) -> Result<Vec<u8>> {
        let path = ObjectPath::from(key);
        self.runtime
            .block_on(async {
                let bytes = self.store.get(&path).await?.bytes().await?;
                Ok::<_, object_store::Error>(bytes.to_vec())
            })
            .map_err(|error| transport_err(&format!("get {key}"), error))
    }

    fn get_range(&self, key: &str, range: Range<u64>) -> Result<Vec<u8>> {
        let path = ObjectPath::from(key);
        // Clamp to the object's actual length so growth-loop callers (see
        // `crate::decode`) can probe with an intentionally generous window
        // without the last object in a pack erroring on out-of-bounds ends.
        self.runtime
            .block_on(async {
                let meta = self.store.head(&path).await?;
                let len = meta.size;
                let start = range.start.min(len);
                let end = range.end.min(len);
                if start >= end {
                    return Ok::<_, object_store::Error>(Vec::new());
                }
                let bytes = self.store.get_range(&path, start..end).await?;
                Ok(bytes.to_vec())
            })
            .map_err(|error| transport_err(&format!("get_range {key}"), error))
    }

    fn exists(&self, key: &str) -> Result<bool> {
        let path = ObjectPath::from(key);
        self.runtime.block_on(async {
            match self.store.head(&path).await {
                Ok(_meta) => Ok(true),
                Err(object_store::Error::NotFound { .. }) => Ok(false),
                Err(error) => Err(transport_err(&format!("exists {key}"), error)),
            }
        })
    }

    fn delete(&self, key: &str) -> Result<()> {
        let path = ObjectPath::from(key);
        self.runtime.block_on(async {
            match self.store.delete(&path).await {
                Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
                Err(error) => Err(transport_err(&format!("delete {key}"), error)),
            }
        })
    }

    fn copy(&self, from: &str, to: &str) -> Result<()> {
        let from_path = ObjectPath::from(from);
        let to_path = ObjectPath::from(to);
        self.runtime
            .block_on(async { self.store.copy(&from_path, &to_path).await })
            .map_err(|error| transport_err(&format!("copy {from} -> {to}"), error))
    }
}
