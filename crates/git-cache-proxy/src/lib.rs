//! An sccache HTTP cache proxy (`docs/scale-out.adoc`, "WS8 — Hydration and
//! toolchains": "sccache: thin GET/PUT proxy. GET = tree-path lookup under
//! the cache namespace; PUT = attested push to a per-key ref with the
//! worker's member key. sccache never learns git.").
//!
//! `GET /{key}` resolves `key` straight to bytes via
//! [`git_backend::RefStore`]/[`git_backend::ObjectStore`] against
//! `refs/cache/sccache/<key>` — the cache-namespace carve-out rule 4
//! grants: evictable, reconstructible, exempt from provenance but not from
//! verification. Verification here *is* the lookup: the ref names an
//! object id, `ObjectStore::read` either has bytes under that id or
//! doesn't, so there is nothing to separately re-verify.
//!
//! `PUT /{key}` lands the request body as a real attested push to that
//! same key's own ref, signed with the worker's member key, driven
//! in-process through [`git_protocol::native::NativeBackend`] — the exact
//! [`git_protocol::IngestPack`] implementation `git-ents-server`'s own
//! `/_native/.../git-receive-pack` endpoint uses
//! (`crates/git-ents-server/src/native_git.rs`), so a cache entry crosses
//! the identical staged-then-connectivity-checked-then-atomic-then-
//! promoted ordering and attestation check, and (unlike shelling out to
//! `git push`) the write's server-signed op record comes back directly
//! rather than being inferred. Per-key refs (one ref per cache key, never
//! a shared mutable one) are the concurrent-writer story rule 4 calls for:
//! two workers racing to cache the same compilation unit each land their
//! own ref, no compare-and-swap contention between them. Consolidating the
//! resulting many refs down to a bounded set of packs is a WS9 compaction
//! effect's job, not this proxy's — it only ever adds refs, never merges
//! or deletes them.
//!
//! sccache itself never learns any of this: from its side, this is a plain
//! HTTP GET/PUT key-value service, the shape its `webdav`-type HTTP cache
//! backend already speaks (`SCCACHE_WEBDAV_ENDPOINT`, optionally
//! `SCCACHE_WEBDAV_TOKEN`).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use git_backend::{Expected, ObjectStore as _, PackStream, RefEdit, RefName, RefStore as _};
use git_protocol::attestation::{OpSigner, SshOpSigner};
use git_protocol::native::{BackendResolver, NativeBackend, RepoBackends};
use git_protocol::{IngestPack as _, PushCertificate, PushOutcome, PushRequest, RepoId};

/// The ref namespace sccache entries land under: one ref per key,
/// `refs/cache/sccache/<key>`.
pub const CACHE_NS: &str = "refs/cache/sccache";

/// Configuration for [`router`].
pub struct Config {
    /// The bare repository cache entries are read from and pushed to.
    pub repo: PathBuf,
    /// The worker's own SSH signing key — "the worker's member key" per
    /// the design doc — used both to sign every `PUT`'s push certificate
    /// (proving a `PUT` speaks for an enrolled member) and, as this
    /// proxy's own [`OpSigner`], the resulting op record (proving the push
    /// was accepted). A real deployment with a separate server identity
    /// would split these; this proxy has exactly one identity to offer, so
    /// it plays both roles rather than inventing a second key this crate
    /// has no way to provision. `None` disables `PUT` (`405 Method Not
    /// Allowed`) — a proxy with no key cannot attest a write, so refusing
    /// beats silently downgrading to an unattested one.
    pub signing_key: Option<PathBuf>,
    /// Bearer token sccache must present as `Authorization: Bearer
    /// <token>`, matching the credential shape sccache's own `webdav`
    /// cache backend supports (`SCCACHE_WEBDAV_TOKEN`). `None` disables the
    /// check, for a proxy already bound to a private network with no
    /// exposed port.
    pub token: Option<String>,
}

/// This proxy's request counters, exposed the same way
/// [`odb_baked::BakedTier::counters`] exposes its own — an operator-visible
/// surface, not just a debugging aid.
#[derive(Debug, Default)]
pub struct Counters {
    /// `GET`s that found a cache entry.
    pub hits: AtomicU64,
    /// `GET`s that found no cache entry (`404`).
    pub misses: AtomicU64,
    /// `PUT`s that landed a new attested push.
    pub puts: AtomicU64,
}

#[derive(Clone)]
struct AppState {
    config: Arc<Config>,
    counters: Arc<Counters>,
}

/// Build the sccache proxy's [`Router`] (`GET`/`PUT /{*key}`) and a handle
/// to its request counters.
#[must_use = "the router must be mounted (e.g. `axum::serve`) or the counters handle is useless"]
pub fn router(config: Config) -> (Router, Arc<Counters>) {
    let counters = Arc::new(Counters::default());
    let state = AppState {
        config: Arc::new(config),
        counters: counters.clone(),
    };
    let router = Router::new()
        .route("/{*key}", get(get_object).put(put_object))
        .with_state(state);
    (router, counters)
}

/// Whether `headers` carries the bearer token `config` requires, or `true`
/// unconditionally when `config.token` is unset.
fn authorized(config: &Config, headers: &HeaderMap) -> bool {
    let Some(token) = &config.token else {
        return true;
    };
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|presented| presented == token)
}

/// The ref for cache key `key`, or `None` if any `/`-separated segment
/// fails [`git_store::ref_segment_ok`] — rejecting a `..` segment, an empty
/// segment, or anything else hostile to a git ref name before it ever
/// becomes one.
fn cache_ref(key: &str) -> Option<String> {
    let segments: Vec<&str> = key.split('/').collect();
    if segments.is_empty()
        || segments
            .iter()
            .any(|segment| !git_store::ref_segment_ok(segment))
    {
        return None;
    }
    Some(format!("{CACHE_NS}/{}", segments.join("/")))
}

async fn get_object(
    State(state): State<AppState>,
    AxumPath(key): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    if !authorized(&state.config, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(refname) = cache_ref(&key) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let repo = state.config.repo.clone();
    let outcome = tokio::task::spawn_blocking(move || read_entry(&repo, &refname)).await;
    match outcome {
        Ok(Ok(Some(bytes))) => {
            state.counters.hits.fetch_add(1, Ordering::Relaxed);
            (StatusCode::OK, bytes).into_response()
        }
        Ok(Ok(None)) => {
            state.counters.misses.fetch_add(1, Ordering::Relaxed);
            StatusCode::NOT_FOUND.into_response()
        }
        Ok(Err(_)) | Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// Resolve `refname` via [`refstore_files::FilesRefStore`] then read its
/// object via [`odb_files::OdbFiles`] — the plain `RefStore`/`ObjectStore`
/// read path the module doc calls out, no attested-push machinery
/// involved (unlike [`write_entry`]: a read needs no attestation, only
/// verification, and content-addressed lookup by object id already is
/// that).
fn read_entry(repo: &Path, refname: &str) -> Result<Option<Vec<u8>>, git_backend::Error> {
    let refs = refstore_files::FilesRefStore::open(repo)?;
    let Some(oid) = refs.get(&RefName::new(refname))? else {
        return Ok(None);
    };
    let odb = odb_files::OdbFiles::open(repo)?;
    let object = odb.read(oid)?;
    Ok(Some(object.data))
}

async fn put_object(
    State(state): State<AppState>,
    AxumPath(key): AxumPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !authorized(&state.config, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(signing_key) = state.config.signing_key.clone() else {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    };
    let Some(refname) = cache_ref(&key) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let repo = state.config.repo.clone();
    let body = body.to_vec();
    let outcome =
        tokio::task::spawn_blocking(move || write_entry(&repo, &refname, &body, &signing_key))
            .await;
    match outcome {
        Ok(Ok(())) => {
            state.counters.puts.fetch_add(1, Ordering::Relaxed);
            StatusCode::OK.into_response()
        }
        Ok(Err(_)) | Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// Resolves every push against the one configured repository's real
/// `refstore-files`/`odb-files` backends and its currently enrolled
/// members — the same resolution `git-ents-server`'s own native-protocol
/// endpoint performs (`DiskResolver` in
/// `crates/git-ents-server/src/native_git.rs`), loaded fresh on every call
/// so a member enrolled or revoked between two `PUT`s is picked up
/// immediately.
struct DiskResolver {
    repo: PathBuf,
}

impl BackendResolver for DiskResolver {
    fn resolve(&self, _repo: &RepoId) -> git_protocol::Result<RepoBackends> {
        let refs = refstore_files::FilesRefStore::open(&self.repo)
            .map_err(|error| git_protocol::Error::UnknownRepo(error.to_string()))?;
        let objects = odb_files::OdbFiles::open(&self.repo)
            .map_err(|error| git_protocol::Error::UnknownRepo(error.to_string()))?;
        let members = git_member::members::load_all(&self.repo)
            .map_err(|error| git_protocol::Error::UnknownRepo(error.to_string()))?;
        let revoked = git_member::revocations::fingerprints(&self.repo)
            .map_err(|error| git_protocol::Error::UnknownRepo(error.to_string()))?;
        let config = git_ents_core::config::load(&self.repo)
            .map_err(|error| git_protocol::Error::UnknownRepo(error.to_string()))?;
        Ok(RepoBackends {
            refs: Arc::new(refs),
            objects: Arc::new(objects),
            authorized_members: git_member::members::without_revoked(members, &revoked),
            config,
            // This proxy targets the local-disk backend only; wiring
            // reachability artifacts is future work, same caveat
            // `DiskResolver` in `git-ents-server` carries (absence
            // degrades speed, never answers).
            reachability: git_reachability::ArtifactBundle::empty(),
        })
    }
}

/// Land `body` as `target`'s content: hash it into a blob (never written to
/// disk outside a pack, matching [`git_backend::ObjectStore`]'s "no
/// `write_loose`" rule), build a one-object pack for it, sign a push
/// certificate for `target`'s ref update with `signing_key`, and drive
/// [`NativeBackend::receive`] — the real attested-push path, in-process.
///
/// Idempotent, not force-pushed: if `target` already holds a *different*
/// blob than `body` hashes to, the ref update's `Expected::MustNotExist`
/// (or, if some other write raced ahead of us, `NativeBackend::receive`'s
/// own compare-and-swap) refuses it rather than clobbering whatever
/// another worker already cached under the same key. A same-content
/// re-`PUT` of an already-cached key is a checked no-op.
fn write_entry(repo: &Path, target: &str, body: &[u8], signing_key: &Path) -> Result<(), String> {
    let oid = gix_object::compute_hash(gix_hash::Kind::Sha1, gix_object::Kind::Blob, body)
        .map_err(|error| format!("could not hash the cache entry: {error}"))?;

    let refs = refstore_files::FilesRefStore::open(repo).map_err(|error| error.to_string())?;
    match git_backend::RefStore::get(&refs, &RefName::new(target))
        .map_err(|error| error.to_string())?
    {
        Some(existing) if existing == oid => return Ok(()),
        Some(_different) => {
            return Err(format!("{target} already holds a different cache entry"));
        }
        None => {}
    }

    let certificate = sign_push_cert(target, oid, signing_key)?;
    let pack = git_protocol::pack::build_pack(&[git_protocol::pack::PackObject {
        id: oid,
        kind: gix_object::Kind::Blob,
        data: body.to_vec(),
    }])
    .map_err(|error| error.to_string())?;

    let signer: Arc<dyn OpSigner> = Arc::new(SshOpSigner::new(signing_key.to_owned()));
    let backend = NativeBackend::new(
        DiskResolver {
            repo: repo.to_owned(),
        },
        signer,
    );
    let push = PushRequest {
        repo: RepoId::new("cache-proxy"),
        ref_edits: vec![RefEdit {
            name: RefName::new(target),
            expected: Expected::MustNotExist,
            new: Some(oid),
        }],
        pack: PackStream::new(std::io::Cursor::new(pack)),
        push_cert: Some(PushCertificate::new(certificate)),
    };
    match backend.receive(push).map_err(|error| error.to_string())? {
        PushOutcome::Accepted { .. } => Ok(()),
        PushOutcome::Rejected { reason } => Err(reason),
    }
}

/// Sign a minimal, real push certificate — `certificate version 0.1`
/// payload for the single ref update `target` from unborn to `oid`, then an
/// `ssh-keygen -Y sign -n git` signature block — the same shape
/// [`git_signed_push::verify_certificate`] parses regardless of which write
/// path produced it. The `pusher`/`pushee` fields are cosmetic:
/// `git-member`'s `allowed_signers` lines use a wildcard principal, so
/// verification never depends on their content, only on the signature
/// itself matching an enrolled key.
fn sign_push_cert(
    target: &str,
    oid: gix_hash::ObjectId,
    signing_key: &Path,
) -> Result<String, String> {
    let null = gix_hash::ObjectId::null(gix_hash::Kind::Sha1);
    let payload = format!(
        "certificate version 0.1\npusher git-ents-cache-proxy <cache-proxy@git-ents>\npushee cache-proxy\nnonce \n\n{null} {oid} {target}\n"
    );
    let dir = tempfile::tempdir().map_err(|error| format!("could not create temp dir: {error}"))?;
    let payload_path = dir.path().join("payload");
    std::fs::write(&payload_path, &payload)
        .map_err(|error| format!("could not write the push certificate payload: {error}"))?;
    let status = Command::new("ssh-keygen")
        .args(["-q", "-Y", "sign", "-n", "git", "-f"])
        .arg(signing_key)
        .arg(&payload_path)
        .status()
        .map_err(|error| format!("could not run ssh-keygen: {error}"))?;
    if !status.success() {
        return Err("could not sign the push certificate".to_owned());
    }
    let signature = std::fs::read_to_string(dir.path().join("payload.sig"))
        .map_err(|error| format!("could not read the push certificate signature: {error}"))?;
    Ok(format!("{payload}{signature}"))
}
