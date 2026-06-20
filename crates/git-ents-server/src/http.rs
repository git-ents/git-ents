//! Smart-HTTP gateway: delegates the git protocol to `git http-backend`.
//!
//! Every request is handed to git's `http-backend` CGI, which implements the
//! full smart-HTTP protocol (running `git-upload-pack` for fetch and
//! `git-receive-pack` for push). This module only translates between
//! `tiny_http` requests/responses and the CGI's stdin/stdout.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tiny_http::{Header, Request, Response};

const CGI_HEADER_SEP: &[u8] = b"\r\n\r\n";

/// Delegate a single request to `git http-backend` and reply with its output.
pub fn handle(mut request: Request, data_dir: &Path) -> std::io::Result<()> {
    let url = request.url().to_owned();
    let (path_info, query_string) = match url.split_once('?') {
        Some((path, query)) => (path.to_owned(), query.to_owned()),
        None => (url, String::new()),
    };

    if path_info.contains("..") {
        return request.respond(Response::from_string("bad request").with_status_code(400));
    }

    let method = request.method().as_str().to_owned();
    let content_type = header_value(&request, "Content-Type");
    let content_length = header_value(&request, "Content-Length");

    // A push begins with `info/refs?service=git-receive-pack`; auto-init the
    // bare repo so the very first request finds it.
    let is_push = query_string.contains("service=git-receive-pack")
        || path_info.ends_with("/git-receive-pack");
    let push_repo = is_push.then(|| repo_dir(data_dir, &path_info)).flatten();
    if let Some(repo) = &push_repo
        && !repo.exists()
        && let Err(e) = init_bare_repo(repo)
    {
        return request
            .respond(Response::from_string(format!("init failed: {e}")).with_status_code(500));
    }

    let mut body = Vec::new();
    request.as_reader().read_to_end(&mut body)?;

    let mut cmd = Command::new("git");
    cmd.arg("http-backend")
        .env("GIT_PROJECT_ROOT", data_dir)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("PATH_INFO", &path_info)
        .env("QUERY_STRING", &query_string)
        .env("REQUEST_METHOD", &method)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(value) = &content_type {
        cmd.env("CONTENT_TYPE", value);
    }
    if let Some(value) = &content_length {
        cmd.env("CONTENT_LENGTH", value);
    }

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            return request.respond(
                Response::from_string(format!("spawn failed: {e}")).with_status_code(500),
            );
        }
    };

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(&body)?;
    }

    let output = child.wait_with_output()?;

    // A fresh bare repo's `HEAD` points at its initial branch, which may not be
    // the branch the client just pushed; without a valid `HEAD`, clones check
    // out nothing. Adopt a pushed branch so the repo stays clonable.
    if let Some(repo) = &push_repo
        && output.status.success()
    {
        reconcile_head(repo);
    }

    request.respond(build_response(&output.stdout))
}

/// Translate a CGI response (header block, blank line, body) into HTTP.
fn build_response(stdout: &[u8]) -> Response<std::io::Cursor<Vec<u8>>> {
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
    let mut headers: Vec<Header> = Vec::new();
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
            // tiny_http sets this from the body length itself.
        } else if let Ok(header) = Header::from_bytes(name, value) {
            headers.push(header);
        }
    }

    let mut response = Response::from_data(body.to_vec()).with_status_code(status);
    for header in headers {
        response.add_header(header);
    }
    response
}

/// Resolve the bare repository directory from the first path segment.
fn repo_dir(data_dir: &Path, path_info: &str) -> Option<PathBuf> {
    let first = path_info.trim_start_matches('/').split('/').next()?;
    if first.is_empty() {
        return None;
    }
    Some(data_dir.join(first))
}

/// Create a bare repo that accepts pushes over smart-HTTP.
fn init_bare_repo(repo: &Path) -> std::io::Result<()> {
    let init = Command::new("git")
        .arg("init")
        .arg("--bare")
        .arg("-b")
        .arg("main")
        .arg(repo)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
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
        .status()?;
    if !config.success() {
        return Err(std::io::Error::other("git config http.receivepack failed"));
    }
    Ok(())
}

/// Point `HEAD` at a real branch when it dangles after a push.
///
/// Best-effort: the push already succeeded, so failures here are ignored.
fn reconcile_head(repo: &Path) {
    let head_valid = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", "--quiet", "HEAD"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
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
        .status();
}

fn header_value(request: &Request, field: &str) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|header| header.field.as_str().as_str().eq_ignore_ascii_case(field))
        .map(|header| header.value.as_str().to_owned())
}

fn parse_status(value: &[u8]) -> Option<u16> {
    let token = value.split(|byte| *byte == b' ').next()?;
    std::str::from_utf8(token).ok()?.parse().ok()
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|window| window == needle)
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
