//! Git Ents server — helpful guardians of your git trees.

mod http;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{CommandFactory, Parser};

#[derive(Parser)]
#[command(
    name = "git-ents-server",
    about = "Helpful guardians of your git trees."
)]
struct Args {
    /// Generate man pages into the given directory.
    #[arg(long, value_name = "DIR")]
    generate_man: Option<PathBuf>,

    /// Port to listen on.
    #[arg(long, env = "PORT", default_value = "8080")]
    port: u16,

    /// Directory holding the bare repositories served over HTTP.
    #[arg(long, env = "GIT_PROJECT_ROOT", default_value = "/data/repos")]
    data_dir: PathBuf,

    /// Bearer token required for git operations; when unset, auth is disabled.
    #[arg(long, env = "ACCESS_TOKEN")]
    access_token: Option<String>,

    /// Stop after handling this many requests.
    #[arg(long)]
    max_requests: Option<usize>,
}

fn main() -> ExitCode {
    let args = Args::parse();

    if let Some(dir) = args.generate_man {
        let cmd = Args::command();
        if let Err(e) = clap_mangen::generate_to(cmd, dir) {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
        return ExitCode::SUCCESS;
    }

    let server = match tiny_http::Server::http(format!("0.0.0.0:{}", args.port)) {
        Ok(server) => server,
        Err(e) => {
            eprintln!("error: failed to bind to port {}: {e}", args.port);
            return ExitCode::FAILURE;
        }
    };

    let mut count: usize = 0;
    for request in server.incoming_requests() {
        if is_health(&request) {
            let _health = request.respond(tiny_http::Response::from_string("ok"));
        } else if args
            .access_token
            .as_deref()
            .is_some_and(|token| !authorized(&request, token))
        {
            let _denied = request.respond(unauthorized());
        } else if let Err(e) = http::handle(request, &args.data_dir) {
            eprintln!("error: {e}");
        }
        count = count.saturating_add(1);
        if args.max_requests.is_some_and(|max| count >= max) {
            break;
        }
    }

    ExitCode::SUCCESS
}

/// A liveness probe (and the `/` root) that does not touch git.
fn is_health(request: &tiny_http::Request) -> bool {
    matches!(request.url(), "/" | "/healthz")
}

/// Check the request's HTTP Basic credentials against the expected token.
///
/// As with GitHub's HTTPS git auth, the username is ignored and the password
/// field carries the bearer token.
fn authorized(request: &tiny_http::Request, expected: &str) -> bool {
    let Some(value) = http::header_value(request, "Authorization") else {
        return false;
    };
    let Some(encoded) = value
        .strip_prefix("Basic ")
        .or_else(|| value.strip_prefix("basic "))
    else {
        return false;
    };
    let Some(decoded) = base64_decode(encoded.trim()) else {
        return false;
    };
    let Some(colon) = decoded.iter().position(|byte| *byte == b':') else {
        return false;
    };
    decoded.get(colon.saturating_add(1)..) == Some(expected.as_bytes())
}

/// A `401` carrying the Basic challenge git expects before retrying with creds.
fn unauthorized() -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    let mut response = tiny_http::Response::from_string("unauthorized").with_status_code(401);
    if let Ok(header) = tiny_http::Header::from_bytes(
        &b"WWW-Authenticate"[..],
        &br#"Basic realm="git-ents""#[..],
    ) {
        response.add_header(header);
    }
    response
}

/// Decode standard base64 with optional `=` padding; `None` on any bad input.
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    fn sextet(byte: u8) -> Option<u32> {
        let value = u32::from(byte);
        match byte {
            b'A'..=b'Z' => Some(value.saturating_sub(u32::from(b'A'))),
            b'a'..=b'z' => Some(value.saturating_sub(u32::from(b'a')).saturating_add(26)),
            b'0'..=b'9' => Some(value.saturating_sub(u32::from(b'0')).saturating_add(52)),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }

    let bytes: &[u8] = input.trim_end_matches('=').as_bytes();
    let mut out = Vec::new();
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for &byte in bytes {
        acc = (acc << 6) | sextet(byte)?;
        bits = bits.saturating_add(6);
        if bits >= 8 {
            bits = bits.saturating_sub(8);
            out.push(u8::try_from((acc >> bits) & 0xFF).ok()?);
        }
    }
    Some(out)
}
