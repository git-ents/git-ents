//! Smart-HTTP gateway: delegates the git protocol to `git http-backend`.
//!
//! Every request is handed to git's `http-backend` CGI, which implements the
//! full smart-HTTP protocol (running `git-upload-pack` for fetch and
//! `git-receive-pack` for push). This module only translates between Axum
//! requests/responses and the CGI's stdin/stdout.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::AppState;

const CGI_HEADER_SEP: &[u8] = b"\r\n\r\n";

// @relation(deploy.health)
/// A liveness probe (and the `/` root) that does not touch git.
pub async fn health() -> &'static str {
    "ok"
}

// @relation(protocol.routing)
/// Serve a GET: the HTML web UI for browser requests, or `git http-backend` for
/// a git wire-protocol read (the ref advertisement or a dumb-HTTP object fetch).
pub async fn get_request(State(state): State<AppState>, uri: Uri, headers: HeaderMap) -> Response {
    let path_info = uri.path().to_owned();
    let query_string = uri.query().unwrap_or_default().to_owned();

    if path_info.contains("..") {
        return (StatusCode::BAD_REQUEST, "bad request").into_response();
    }

    // Anything that is not part of the git wire protocol is served the HTML web
    // UI rather than handed to the CGI backend.
    if is_web_get(&path_info, &query_string) {
        let host = header_value(&headers, "Host");
        let cookie = header_value(&headers, "Cookie");
        let referer = header_value(&headers, "Referer");
        let query = (!query_string.is_empty()).then_some(query_string.as_str());
        return crate::web::render(
            &state,
            &path_info,
            query,
            host.as_deref(),
            cookie.as_deref(),
            referer.as_deref(),
        )
        .await;
    }

    backend(
        &state,
        Method::GET,
        &path_info,
        &query_string,
        &headers,
        Bytes::new(),
    )
    .await
}

// @relation(protocol.routing)
/// Serve a POST: always a git smart-HTTP RPC (`git-upload-pack` for fetch or
/// `git-receive-pack` for push). The browser UI never POSTs, so there is no web
/// branch here.
pub async fn post_request(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path_info = uri.path().to_owned();
    let query_string = uri.query().unwrap_or_default().to_owned();

    if path_info.contains("..") {
        return (StatusCode::BAD_REQUEST, "bad request").into_response();
    }

    // The browser UI POSTs to sign in, sign out, and save edits; only the two
    // git smart-HTTP RPCs go to the backend.
    if !is_git_post(&path_info) {
        return crate::web::handle_post(&state, &path_info, &headers, body).await;
    }

    backend(
        &state,
        Method::POST,
        &path_info,
        &query_string,
        &headers,
        body,
    )
    .await
}

// @relation(protocol.routing)
/// Whether a POST is a git smart-HTTP RPC rather than a browser form submission.
fn is_git_post(path_info: &str) -> bool {
    path_info.ends_with("/git-upload-pack") || path_info.ends_with("/git-receive-pack")
}

// @relation(protocol.git, compat.git, compat.cgi, nonfunctional.concurrency)
/// Hand a git wire-protocol request to `git http-backend` and reply with its
/// output. A receive-pack request (push) auto-creates its bare repository before
/// the backend runs and reconciles `HEAD` after a successful push.
async fn backend(
    state: &AppState,
    method: Method,
    path_info: &str,
    query_string: &str,
    headers: &HeaderMap,
    body: Bytes,
) -> Response {
    // A push uses exactly two endpoints: the receive-pack advertisement
    // (`GET /<repo>/info/refs?service=git-receive-pack`) and the receive-pack
    // RPC (`POST /<repo>/git-receive-pack`). Recognize the target so the bare
    // repo can be auto-created on the very first request; reject an
    // unacceptable repository path here rather than handing it to the backend.
    let push_repo = if is_receive_pack(path_info, query_string) {
        match repo_path(path_info) {
            Some(relative) => Some(state.data_dir.join(relative)),
            None => return (StatusCode::BAD_REQUEST, "invalid repository path").into_response(),
        }
    } else {
        None
    };
    if let Some(repo) = &push_repo
        && let Err(response) = ensure_repo(state, repo).await
    {
        return response;
    }

    // WS0 hydration (`docs/scale-out.adoc`): before handing this request to
    // `git http-backend`, top up the ephemeral local repo from the durable
    // stores and, for anything answering a ref advertisement, regenerate
    // `packed-refs` from Postgres — bounding advertisement staleness to
    // this one request. A no-op (and `state.hydrate` is `None`) for a
    // local-only deployment.
    if let Some(hydrate) = &state.hydrate
        && is_service_request(path_info, query_string)
        && let Some(relative) = repo_path(path_info)
    {
        let repo_path = state.data_dir.join(&relative);
        let repo_id = repo_id_string(&relative);
        if let Err(response) = hydrate_repo(hydrate, &repo_path, &repo_id).await {
            return response;
        }
    }

    let content_type = header_value(headers, "Content-Type");
    let content_length = header_value(headers, "Content-Length");
    // `Content-Type`/`Content-Length` are CGI-special-cased env vars with no
    // `HTTP_` prefix; every other header (including `Content-Encoding`) maps to
    // `HTTP_<NAME>`. Without it, a gzip'd request body — which git's client
    // sends once a negotiation grows past its size threshold, e.g. `fetch`ing
    // many refs at once — reaches `git-upload-pack` still compressed, which it
    // reads as raw (garbled) pkt-lines: "protocol error: bad line length
    // character".
    let content_encoding = header_value(headers, "Content-Encoding");

    let mut cmd = Command::new("git");
    cmd.arg("http-backend")
        .env("GIT_PROJECT_ROOT", &state.data_dir)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("PATH_INFO", path_info)
        .env("QUERY_STRING", query_string)
        .env("REQUEST_METHOD", method.as_str())
        // Hand the `post-receive` hook the queue it drops jobs into; it inherits
        // this through the receive-pack process tree git spawns.
        .env(git_effect::engine::QUEUE_ENV, &state.checks_queue)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Surface backend diagnostics in the server's own logs rather than
        // discarding them; git http-backend only writes here when something
        // goes wrong.
        .stderr(Stdio::inherit());
    if let Some(value) = &content_type {
        cmd.env("CONTENT_TYPE", value);
    }
    if let Some(value) = &content_length {
        cmd.env("CONTENT_LENGTH", value);
    }
    if let Some(value) = &content_encoding {
        cmd.env("HTTP_CONTENT_ENCODING", value);
    }
    // Hand the hydration-mode `pre-receive` (`git_hydrate::pre_receive`) the
    // durable-store config it needs: these env vars propagate through
    // `http-backend` -> `receive-pack` -> the hook exactly as
    // `git_effect::engine::QUEUE_ENV` above already does, and
    // `git_hydrate::HydrateConfig::from_env` reads them back.
    if let Some(hydrate) = &state.hydrate {
        // The op record's signer: `git_hydrate::pre_receive` needs the same
        // key `AppState::web_signing_key` already holds in this process,
        // but the hook is a separate process with no access to it.
        if let Some(key) = &state.web_signing_key {
            cmd.env("GIT_ENTS_WEB_SIGNING_KEY", key);
        }
        cmd.env("GIT_ENTS_HYDRATE_POSTGRES_URL", &hydrate.postgres_conninfo);
        match &hydrate.blob {
            git_hydrate::config::BlobStore::Fs(root) => {
                cmd.env("GIT_ENTS_HYDRATE_BLOB_ROOT", root);
            }
            git_hydrate::config::BlobStore::S3(s3) => {
                cmd.env("GIT_ENTS_HYDRATE_S3_BUCKET", &s3.bucket)
                    .env("GIT_ENTS_HYDRATE_S3_REGION", &s3.region)
                    .env("GIT_ENTS_HYDRATE_S3_ENDPOINT", &s3.endpoint)
                    .env("GIT_ENTS_HYDRATE_S3_ACCESS_KEY_ID", &s3.access_key_id)
                    .env(
                        "GIT_ENTS_HYDRATE_S3_SECRET_ACCESS_KEY",
                        &s3.secret_access_key,
                    );
                if s3.allow_http {
                    cmd.env("GIT_ENTS_HYDRATE_S3_ALLOW_HTTP", "1");
                }
            }
        }
    }

    // Push these through `GIT_CONFIG_*` rather than `git -c` so they reach the
    // `receive-pack` and `pre-receive` processes http-backend spawns, where the
    // nonce and hook actually take effect.
    let overrides = backend_config(state);
    if !overrides.is_empty() {
        cmd.env("GIT_CONFIG_COUNT", overrides.len().to_string());
        for (index, (key, value)) in overrides.iter().enumerate() {
            cmd.env(format!("GIT_CONFIG_KEY_{index}"), key);
            cmd.env(format!("GIT_CONFIG_VALUE_{index}"), value);
        }
    }

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("spawn failed: {e}"),
            )
                .into_response();
        }
    };

    // Feed the request body and drain stdout concurrently; receive-pack streams
    // progress to stdout while still reading the pack, so a sequential
    // write-then-read would deadlock on a full pipe.
    let writer = child.stdin.take().map(|mut stdin| {
        tokio::spawn(async move {
            let _write = stdin.write_all(&body).await;
            // `stdin` drops here, closing the pipe so the CGI sees EOF.
        })
    });

    let mut stdout = Vec::new();
    if let Some(mut out) = child.stdout.take() {
        let _read = out.read_to_end(&mut stdout).await;
    }
    if let Some(writer) = writer {
        let _joined = writer.await;
    }

    let status = match child.wait().await {
        Ok(status) => status,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("backend failed: {e}"),
            )
                .into_response();
        }
    };

    // A fresh bare repo's `HEAD` points at its initial branch, which may not be
    // the branch the client just pushed; without a valid `HEAD`, clones check
    // out nothing. Adopt a pushed branch so the repo stays cloneable.
    if let Some(repo) = &push_repo
        && status.success()
    {
        reconcile_head(repo).await;
    }

    build_response(&stdout)
}

// @relation(compat.git, auth.nonce)
/// The `git` config overrides applied to every backend invocation. Empty until
/// push authentication is wired: a seed enables the signed-push nonce, and the
/// hooks directory points the backend at the `pre-receive` verifier.
fn backend_config(state: &AppState) -> Vec<(&'static str, &str)> {
    let mut overrides = Vec::new();
    if let Some(seed) = state.cert_nonce_seed.as_deref() {
        overrides.push(("receive.certNonceSeed", seed));
        // Smart-HTTP issues the nonce and verifies it in two separate
        // `receive-pack` processes, so the cert's stamp never matches the
        // verifier's "now"; without a slop window git's default of 0 returns
        // SLOP for every signed push. Allow a small drift for the round-trip.
        overrides.push(("receive.certNonceSlop", "60"));
    }
    if let Some(hooks) = state.hooks_dir.as_deref().and_then(Path::to_str) {
        overrides.push(("core.hooksPath", hooks));
    }
    overrides
}

// @relation(compat.cgi)
/// Translate a CGI response (header block, blank line, body) into HTTP.
fn build_response(stdout: &[u8]) -> Response {
    let (header_block, body) = match find_subsequence(stdout, CGI_HEADER_SEP) {
        Some(pos) => {
            let body_start = pos.saturating_add(CGI_HEADER_SEP.len());
            (
                stdout.get(..pos).unwrap_or_default(),
                stdout.get(body_start..).unwrap_or_default(),
            )
        }
        None => (&b""[..], stdout),
    };

    let mut status = 200u16;
    let mut builder = Response::builder();
    for raw in header_block.split(|byte| *byte == b'\n') {
        let line = trim_cr(raw);
        let Some(colon) = line.iter().position(|byte| *byte == b':') else {
            continue;
        };
        let name = line.get(..colon).unwrap_or_default();
        let value = trim_space(line.get(colon.saturating_add(1)..).unwrap_or_default());
        if name.eq_ignore_ascii_case(b"Status") {
            status = parse_status(value).unwrap_or(200);
        } else if name.eq_ignore_ascii_case(b"Content-Length") {
            // Axum sets this from the body length itself.
        } else {
            builder = builder.header(name, value);
        }
    }

    builder
        .status(status)
        .body(Body::from(body.to_vec()))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// Greatest repository nesting depth: `repo`, `org/repo`, or `org/team/repo`.
/// Shared by the push gateway and the web UI's routing/discovery.
pub(crate) const MAX_REPO_DEPTH: usize = 3;

// @relation(protocol.routing)
/// Whether a GET should be answered with the HTML web UI rather than handed to
/// `git http-backend`.
///
/// Anything that is not a git wire-protocol request (smart or dumb HTTP) is web.
/// In addition, the browse routes (`/tree/`, `/blob/`, `/commit/`) are claimed
/// for the web UI even when a file path within them happens to resemble a
/// dumb-HTTP git path (e.g. a file named `HEAD`, or a directory named
/// `objects`) — but never when it is an actual smart-HTTP service request, so a
/// repository named `commit` can still be pushed to and cloned.
fn is_web_get(path: &str, query: &str) -> bool {
    let is_wire =
        is_service_request(path, query) || path.ends_with("/HEAD") || path.contains("/objects/");
    let is_browse = path.contains("/tree/") || path.contains("/blob/") || path.contains("/commit/");
    !is_wire || (is_browse && !is_service_request(path, query))
}

// @relation(protocol.routing)
/// Whether `path`/`query` is an unambiguous smart-HTTP service request (the
/// ref advertisement or an upload-pack/receive-pack RPC).
fn is_service_request(path: &str, query: &str) -> bool {
    path.ends_with("/info/refs")
        || path.ends_with("/git-upload-pack")
        || path.ends_with("/git-receive-pack")
        || query.contains("service=")
}

/// Whether this request is a push: the smart-HTTP receive-pack advertisement
/// (`/info/refs?service=git-receive-pack`) or the receive-pack RPC itself.
///
/// The `service` parameter is matched exactly, so a request for the read-only
/// `git-upload-pack` service (or an unrelated parameter that merely contains
/// the string) is not mistaken for a push.
fn is_receive_pack(path_info: &str, query: &str) -> bool {
    path_info.ends_with("/git-receive-pack")
        || (path_info.ends_with("/info/refs") && query_service(query) == Some("git-receive-pack"))
}

/// The value of the `service` query parameter, if present.
pub(crate) fn query_service(query: &str) -> Option<&str> {
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix("service="))
}

// @relation(namespace.auto-create, storage.bare)
/// Ensure `repo` exists as a bare repository, creating it on first push.
///
/// Holds [`AppState::init_lock`] across the whole check-and-create so two
/// concurrent first pushes to the same name cannot both initialize it, and
/// refuses paths that collide with an existing repository: one nested inside a
/// repo, or one that already exists as a namespace directory.
pub(crate) async fn ensure_repo(state: &AppState, repo: &Path) -> Result<(), Response> {
    let _guard = state.init_lock.lock().await;
    if enclosing_repo(&state.data_dir, repo).is_some() {
        return Err((
            StatusCode::CONFLICT,
            "repository path is nested inside an existing repository",
        )
            .into_response());
    }
    if repo.exists() {
        return if is_bare_repo(repo) {
            Ok(())
        } else {
            Err((
                StatusCode::CONFLICT,
                "repository path already exists as a namespace",
            )
                .into_response())
        };
    }
    init_bare_repo(repo).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("init failed: {e}"),
        )
            .into_response()
    })
}

// @relation(namespace.path)
/// The ancestor of `repo` (below `data_dir`) that is itself a bare repository,
/// if any. Used to refuse creating a repository inside another one.
fn enclosing_repo(data_dir: &Path, repo: &Path) -> Option<PathBuf> {
    let relative = repo.strip_prefix(data_dir).ok()?;
    let mut current = data_dir.to_path_buf();
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        if components.peek().is_none() {
            break;
        }
        current.push(component);
        if is_bare_repo(&current) {
            return Some(current);
        }
    }
    None
}

/// Whether `path` is the root of a bare git repository.
pub(crate) fn is_bare_repo(path: &Path) -> bool {
    path.join("HEAD").is_file() && path.join("objects").is_dir()
}

// @relation(namespace.path)
/// The target repository of a smart-HTTP service request (push or fetch),
/// as a validated path relative to the data directory, or `None` if the
/// request does not name an acceptable repository.
///
/// The repository is everything before git's service suffix (`/info/refs`,
/// `/git-receive-pack`, or `/git-upload-pack`), limited to
/// [`MAX_REPO_DEPTH`] segments each drawn from a conservative character
/// set. Validating the segments here is what keeps a request from escaping
/// the data directory or fabricating arbitrary paths on disk: every
/// returned component is a plain, dot-free, separator-free name, so the
/// join below can only ever descend into `data_dir`.
fn repo_path(path_info: &str) -> Option<PathBuf> {
    let repo = path_info
        .strip_suffix("/git-receive-pack")
        .or_else(|| path_info.strip_suffix("/git-upload-pack"))
        .or_else(|| path_info.strip_suffix("/info/refs"))?;
    let segments: Vec<&str> = repo.split('/').filter(|s| !s.is_empty()).collect();
    if segments.is_empty() || segments.len() > MAX_REPO_DEPTH {
        return None;
    }
    if !segments.iter().all(|segment| valid_segment(segment)) {
        return None;
    }
    Some(segments.into_iter().collect())
}

// @relation(namespace.path)
/// Whether a single path component is a safe repository/namespace name.
///
/// Rejecting any leading `.` rules out `.`, `..`, and hidden directories; the
/// allow-list of characters rules out path separators, NUL, percent-encoding,
/// and whitespace, so no segment can traverse or otherwise escape on disk.
pub(crate) fn valid_segment(segment: &str) -> bool {
    !segment.is_empty()
        && !segment.starts_with('.')
        && segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

// @relation(namespace.auto-create, compat.git)
/// Create a bare repo that accepts pushes over smart-HTTP.
async fn init_bare_repo(repo: &Path) -> std::io::Result<()> {
    let init = Command::new("git")
        .arg("init")
        .arg("--bare")
        .arg("-b")
        .arg("main")
        .arg(repo)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await?;
    if !init.success() {
        return Err(std::io::Error::other("git init --bare failed"));
    }
    let config = Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("config")
        .arg("http.receivepack")
        .arg("true")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await?;
    if !config.success() {
        return Err(std::io::Error::other("git config http.receivepack failed"));
    }
    Ok(())
}

// @relation(namespace.auto-create, compat.git)
/// Point `HEAD` at a real branch when it dangles after a push.
///
/// Best-effort: the push already succeeded, so failures here are ignored.
async fn reconcile_head(repo: &Path) {
    let head_valid = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", "--quiet", "HEAD"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|status| status.success())
        .unwrap_or(false);
    if head_valid {
        return;
    }

    let Ok(output) = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["for-each-ref", "--format=%(refname:short)", "refs/heads/"])
        .stderr(Stdio::null())
        .output()
        .await
    else {
        return;
    };
    let branches = String::from_utf8_lossy(&output.stdout);
    let branches: Vec<&str> = branches.lines().filter(|line| !line.is_empty()).collect();
    let Some(branch) = branches
        .iter()
        .find(|name| **name == "main")
        .or_else(|| branches.iter().find(|name| **name == "master"))
        .or_else(|| branches.first())
    else {
        return;
    };

    let _set = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["symbolic-ref", "HEAD", &format!("refs/heads/{branch}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}

/// `relative`'s repository id, the way every hydration-mode component
/// (this module, `git_hydrate::pre_receive`, `native_git`'s own resolver)
/// names one: its data-dir-relative path with forward slashes, regardless
/// of host path-separator conventions.
fn repo_id_string(relative: &Path) -> String {
    relative.to_string_lossy().replace('\\', "/")
}

// @relation(protocol.git, storage.bare)
/// WS0's read-path hydration step for one request: top up `repo_path`'s
/// local packs from `hydrate`'s durable stores (idempotent — a no-op past
/// the first pack a given ephemeral instance has already fetched) and
/// regenerate its `packed-refs` from Postgres, bounding advertisement
/// staleness to this one request (`docs/scale-out.adoc`, "WS0").
///
/// Runs on a blocking task: [`refstore_postgres::PostgresRefStore`] and
/// [`odb_tigris::OdbTigris`] are synchronous (each owns its own dedicated
/// runtime for the async clients underneath), so driving them straight from
/// this async handler would block the executor thread they're called from.
async fn hydrate_repo(
    hydrate: &git_hydrate::HydrateConfig,
    repo_path: &Path,
    repo_id: &str,
) -> Result<(), Response> {
    let hydrate = hydrate.clone();
    let repo_path = repo_path.to_path_buf();
    let repo_id = repo_id.to_owned();
    let result = tokio::task::spawn_blocking(move || -> git_backend::Result<()> {
        let registry = refstore_postgres::PostgresRefStore::connect(
            &hydrate.postgres_conninfo,
            repo_id.clone(),
        )?;
        match &hydrate.blob {
            git_hydrate::config::BlobStore::Fs(root) => {
                let transport = odb_tigris::transport::fs::FsTransport::open(root)?;
                git_hydrate::hydrate::ensure_hydrated(&repo_path, &repo_id, &transport, &registry)?;
            }
            git_hydrate::config::BlobStore::S3(s3) => {
                let transport = odb_tigris::transport::s3::S3Transport::connect(s3)?;
                git_hydrate::hydrate::ensure_hydrated(&repo_path, &repo_id, &transport, &registry)?;
            }
        }
        let refs =
            refstore_postgres::PostgresRefStore::connect(&hydrate.postgres_conninfo, repo_id)?;
        git_hydrate::packed_refs::regenerate(&repo_path, &refs)?;
        Ok(())
    })
    .await;
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("hydration failed: {error}"),
        )
            .into_response()),
        Err(join_error) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("hydration task panicked: {join_error}"),
        )
            .into_response()),
    }
}

fn header_value(headers: &HeaderMap, field: &str) -> Option<String> {
    headers
        .get(field)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

fn parse_status(value: &[u8]) -> Option<u16> {
    let token = value.split(|byte| *byte == b' ').next()?;
    std::str::from_utf8(token).ok()?.parse().ok()
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn trim_cr(line: &[u8]) -> &[u8] {
    match line.split_last() {
        Some((b'\r', rest)) => rest,
        _ => line,
    }
}

fn trim_space(mut value: &[u8]) -> &[u8] {
    while let Some((first, rest)) = value.split_first() {
        if *first == b' ' || *first == b'\t' {
            value = rest;
        } else {
            break;
        }
    }
    while let Some((last, rest)) = value.split_last() {
        if *last == b' ' || *last == b'\t' {
            value = rest;
        } else {
            break;
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case("repo", true)]
    #[case("repo.git", true)]
    #[case("My-Repo_1.git", true)]
    #[case("", false)]
    #[case(".", false)]
    #[case("..", false)]
    #[case(".hidden", false)]
    #[case("a/b", false)]
    #[case("a b", false)]
    #[case("a%2eb", false)]
    // @relation(namespace.path, role=Verifies)
    fn validates_segments(#[case] segment: &str, #[case] expected: bool) {
        assert_eq!(valid_segment(segment), expected);
    }

    fn state(cert_nonce_seed: Option<&str>, hooks_dir: Option<&str>) -> AppState {
        AppState {
            data_dir: PathBuf::from("/data"),
            init_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
            cert_nonce_seed: cert_nonce_seed.map(str::to_owned),
            hooks_dir: hooks_dir.map(PathBuf::from),
            checks_queue: PathBuf::from("/data/checks-queue"),
            sessions: crate::web::new_sessions(),
            challenges: crate::web::new_challenges(),
            web_signing_key: None,
            live_runs: git_effect::engine::new_live_registry(),
            hydrate: None,
        }
    }

    // @relation(auth.nonce, role=Verifies)
    #[test]
    fn backend_config_is_empty_without_authentication() {
        assert!(backend_config(&state(None, None)).is_empty());
    }

    // @relation(auth.nonce, role=Verifies)
    #[test]
    fn backend_config_injects_nonce_seed_and_hooks_path() {
        assert_eq!(
            backend_config(&state(Some("seed"), Some("/app/hooks"))),
            vec![
                ("receive.certNonceSeed", "seed"),
                ("receive.certNonceSlop", "60"),
                ("core.hooksPath", "/app/hooks"),
            ]
        );
    }

    #[rstest]
    #[case("/repo.git/git-receive-pack", Some("repo.git"))]
    #[case("/org/repo.git/git-receive-pack", Some("org/repo.git"))]
    #[case("/org/team/repo.git/git-receive-pack", Some("org/team/repo.git"))]
    #[case("/repo.git/info/refs", Some("repo.git"))]
    #[case("/a/b/c/d.git/git-receive-pack", None)]
    #[case("/../etc/git-receive-pack", None)]
    #[case("/.ssh/git-receive-pack", None)]
    #[case("/git-receive-pack", None)]
    // @relation(namespace.path, role=Verifies)
    fn extracts_repo_path(#[case] path: &str, #[case] expected: Option<&str>) {
        assert_eq!(repo_path(path).as_deref(), expected.map(Path::new));
    }

    #[rstest]
    #[case("/repo.git/git-receive-pack", "", true)]
    #[case("/repo.git/info/refs", "service=git-receive-pack", true)]
    #[case("/repo.git/info/refs", "service=git-upload-pack", false)]
    #[case("/repo.git/info/refs", "", false)]
    #[case("/repo.git/info/refs", "x=service=git-receive-pack", false)]
    #[case("/repo.git/info/refs", "a=b&service=git-receive-pack", true)]
    #[case("/repo.git/objects/info/packs", "", false)]
    // @relation(protocol.routing, role=Verifies)
    fn detects_pushes(#[case] path: &str, #[case] query: &str, #[case] expected: bool) {
        assert_eq!(is_receive_pack(path, query), expected);
    }

    #[rstest]
    // Plain browse pages and the index are always web.
    #[case("/", "", true)]
    #[case("/repo", "", true)]
    #[case("/repo/tree/src", "", true)]
    #[case("/repo/blob/src/main.rs", "", true)]
    #[case("/repo/commit/abc123", "", true)]
    // Browse routes win over the loose dumb-HTTP heuristics: a file named HEAD,
    // or a directory named `objects`, still renders as HTML.
    #[case("/repo/blob/HEAD", "", true)]
    #[case("/repo/blob/src/objects/mod.rs", "", true)]
    // Real smart-HTTP requests are never stolen, even for a repo named `commit`.
    #[case("/commit/info/refs", "service=git-upload-pack", false)]
    #[case("/commit/git-receive-pack", "", false)]
    #[case("/repo/info/refs", "service=git-upload-pack", false)]
    #[case("/repo/git-upload-pack", "", false)]
    #[case("/repo/objects/12/abcdef", "", false)]
    // @relation(protocol.routing, role=Verifies)
    fn routes_browser_gets(#[case] path: &str, #[case] query: &str, #[case] expected: bool) {
        assert_eq!(is_web_get(path, query), expected);
    }
}
