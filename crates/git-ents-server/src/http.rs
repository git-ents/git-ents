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

/// A liveness probe (and the `/` root) that does not touch git.
pub async fn health() -> &'static str {
    "ok"
}

/// Delegate a single request to `git http-backend` and reply with its output.
pub async fn git(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path_info = uri.path().to_owned();
    let query_string = uri.query().unwrap_or_default().to_owned();

    if path_info.contains("..") {
        return (StatusCode::BAD_REQUEST, "bad request").into_response();
    }

    // A plain browser GET (anything that is not part of the git wire protocol)
    // is served the HTML web UI rather than handed to the CGI backend.
    if method == Method::GET && wants_web_ui(&path_info, &query_string) {
        let host = header_value(&headers, "Host");
        return crate::web::render(&state, &path_info, host.as_deref()).await;
    }

    // A push uses exactly two endpoints: the receive-pack advertisement
    // (`GET /<repo>/info/refs?service=git-receive-pack`) and the receive-pack
    // RPC (`POST /<repo>/git-receive-pack`). Recognize the target so the bare
    // repo can be auto-created on the very first request; reject an
    // unacceptable repository path here rather than handing it to the backend.
    let push_repo = if is_receive_pack(&path_info, &query_string) {
        match repo_path(&path_info) {
            Some(relative) => Some(state.data_dir.join(relative)),
            None => return (StatusCode::BAD_REQUEST, "invalid repository path").into_response(),
        }
    } else {
        None
    };
    if let Some(repo) = &push_repo
        && let Err(response) = ensure_repo(&state, repo).await
    {
        return response;
    }

    let content_type = header_value(&headers, "Content-Type");
    let content_length = header_value(&headers, "Content-Length");

    let mut cmd = Command::new("git");
    cmd.arg("http-backend")
        .env("GIT_PROJECT_ROOT", &state.data_dir)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("PATH_INFO", &path_info)
        .env("QUERY_STRING", &query_string)
        .env("REQUEST_METHOD", method.as_str())
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
const MAX_REPO_DEPTH: usize = 3;

/// Whether a GET should be answered with the HTML web UI rather than handed to
/// `git http-backend`.
///
/// Anything that is not a git wire-protocol request is web. In addition, the
/// browse routes (`/tree/`, `/blob/`, `/commit/`) are claimed for the web UI
/// even when a file path within them happens to resemble a dumb-HTTP git path
/// (e.g. a file named `HEAD`, or a directory named `objects`) — but never when
/// it is an actual smart-HTTP service request, so a repository named `commit`
/// can still be pushed to and cloned.
fn wants_web_ui(path: &str, query: &str) -> bool {
    !is_git_path(path, query) || (is_browse_route(path) && !is_service_request(path, query))
}

/// Whether `path` selects one of the web UI's browse views.
fn is_browse_route(path: &str) -> bool {
    path.contains("/tree/") || path.contains("/blob/") || path.contains("/commit/")
}

/// Whether `path`/`query` is an unambiguous smart-HTTP service request (the
/// ref advertisement or an upload-pack/receive-pack RPC).
fn is_service_request(path: &str, query: &str) -> bool {
    path.ends_with("/info/refs")
        || path.ends_with("/git-upload-pack")
        || path.ends_with("/git-receive-pack")
        || query.contains("service=")
}

/// Whether `path`/`query` belong to git's wire protocol (smart or dumb HTTP)
/// rather than the browser-facing web UI. Anything matching here is delegated
/// to `git http-backend`; everything else is rendered as HTML.
fn is_git_path(path: &str, query: &str) -> bool {
    path.ends_with("/info/refs")
        || path.ends_with("/git-upload-pack")
        || path.ends_with("/git-receive-pack")
        || path.ends_with("/HEAD")
        || path.contains("/objects/")
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
fn query_service(query: &str) -> Option<&str> {
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix("service="))
}

/// Ensure `repo` exists as a bare repository, creating it on first push.
///
/// Holds [`AppState::init_lock`] across the whole check-and-create so two
/// concurrent first pushes to the same name cannot both initialize it, and
/// refuses paths that collide with an existing repository: one nested inside a
/// repo, or one that already exists as a namespace directory.
async fn ensure_repo(state: &AppState, repo: &Path) -> Result<(), Response> {
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

/// The target repository of a push, as a validated path relative to the data
/// directory, or `None` if the request does not name an acceptable repository.
///
/// The repository is everything before git's service suffix (`/info/refs` or
/// `/git-receive-pack`), limited to [`MAX_REPO_DEPTH`] segments each drawn from
/// a conservative character set. Validating the segments here is what keeps a
/// push from escaping the data directory or fabricating arbitrary paths on
/// disk: every returned component is a plain, dot-free, separator-free name, so
/// the join below can only ever descend into `data_dir`.
fn repo_path(path_info: &str) -> Option<PathBuf> {
    let repo = path_info
        .strip_suffix("/git-receive-pack")
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
    fn validates_segments(#[case] segment: &str, #[case] expected: bool) {
        assert_eq!(valid_segment(segment), expected);
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
    fn routes_browser_gets(#[case] path: &str, #[case] query: &str, #[case] expected: bool) {
        assert_eq!(wants_web_ui(path, query), expected);
    }
}
