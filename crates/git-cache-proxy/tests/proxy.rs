#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "integration test binary"
)]

//! End-to-end coverage for the sccache proxy at the axum level: `PUT` then
//! `GET` round-trips bytes, `GET` on an unknown key 404s, and a `PUT`
//! really lands as an attested per-key cache ref with a server-signed op
//! record — not just a local write.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use git_backend::{RefName, RefStore as _};
use git_cache_proxy::{CACHE_NS, Config, router};
use git_member::members::{Member, Provenance, Trust};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

/// A fresh bare repository, ready for `refstore-files`/`odb-files` to open.
fn bare_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let status = Command::new("git")
        .args(["init", "-q", "--bare", "-b", "main"])
        .arg(dir.path())
        .status()
        .expect("run git init");
    assert!(status.success());
    dir
}

/// A fresh ed25519 keypair at `base/<name>`, returning `(private, public)`
/// paths.
fn keygen(base: &Path, name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let key = base.join(name);
    let status = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", name, "-f"])
        .arg(&key)
        .status()
        .expect("run ssh-keygen");
    assert!(status.success());
    (key.clone(), base.join(format!("{name}.pub")))
}

/// Enroll `public_key` as an admin-registered member of `repo` — the
/// "worker's member key" every `PUT` in this test signs with.
fn enroll(repo: &Path, public_key: &Path) {
    let key = std::fs::read_to_string(public_key).expect("read public key");
    let mut keys = BTreeMap::new();
    keys.insert("worker".to_owned(), key);
    let member = Member {
        principal: "worker".to_owned(),
        valid_after: None,
        valid_before: None,
        trust: Trust::Keys(keys),
        provenance: Provenance::AdminRegistered,
        account: None,
        role: None,
    };
    git_member::members::store(repo, &member).expect("enroll member");
}

/// Send a minimal HTTP/1.1 request over a fresh connection to `addr` and
/// return `(status, body)`. `Connection: close` sidesteps keep-alive
/// bookkeeping — a fresh connection per request is fine for a handful of
/// assertions.
async fn request(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> (u16, Vec<u8>) {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write request");
    stream.write_all(body).await.expect("write body");

    // Deliberately not shutting down our write half here: hyper's server
    // treats a half-closed peer as an abrupt disconnect rather than "done
    // sending, still listening for the response" and drops the connection
    // without writing anything back. `Connection: close` in the request
    // above is enough for the server to close its own side once it has
    // written the response, which is what ends this `read_to_end`.
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.expect("read response");
    let header_end = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap_or_else(|| {
            panic!(
                "response has a header/body separator; raw ({} bytes) = {:?}",
                raw.len(),
                String::from_utf8_lossy(&raw)
            )
        });
    let header_text = String::from_utf8_lossy(&raw[..header_end]);
    let status: u16 = header_text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .expect("status line");
    let response_body = raw[header_end.saturating_add(4)..].to_vec();
    (status, response_body)
}

#[tokio::test(flavor = "multi_thread")]
async fn put_then_get_round_trips_bytes_and_lands_an_attested_cache_ref() {
    let repo = bare_repo();
    let keys_dir = tempfile::tempdir().expect("tempdir");
    let (private, public) = keygen(keys_dir.path(), "worker");
    enroll(repo.path(), &public);

    let config = Config {
        repo: repo.path().to_owned(),
        signing_key: Some(private),
        token: Some("s3cret".to_owned()),
    };
    let (app, counters) = router(config);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });

    let auth = [("Authorization", "Bearer s3cret")];
    let body = b"sccache compiled artifact bytes";

    // GET on an unknown key 404s.
    let (status, _) = request(addr, "GET", "/no-such-key", &auth, b"").await;
    assert_eq!(status, 404);

    // An unauthenticated request is rejected before ever touching the repo.
    let (status, _) = request(addr, "GET", "/some-key", &[], b"").await;
    assert_eq!(status, 401);

    // PUT lands the entry.
    let (status, _) = request(addr, "PUT", "/some-key", &auth, body).await;
    assert_eq!(status, 200, "PUT should succeed");

    // GET returns exactly what was PUT.
    let (status, got) = request(addr, "GET", "/some-key", &auth, b"").await;
    assert_eq!(status, 200);
    assert_eq!(got, body);

    assert_eq!(counters.puts.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert_eq!(counters.hits.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert_eq!(
        counters.misses.load(std::sync::atomic::Ordering::Relaxed),
        1
    );

    // The PUT really landed as a per-key ref under the cache namespace...
    let refs = refstore_files::FilesRefStore::open(repo.path()).expect("open ref store");
    let cache_ref = RefName::new(format!("{CACHE_NS}/some-key"));
    let cached_oid = refs
        .get(&cache_ref)
        .expect("read cache ref")
        .expect("cache ref exists");
    let expected_oid =
        gix_object::compute_hash(gix_hash::Kind::Sha1, gix_object::Kind::Blob, body).expect("hash");
    assert_eq!(cached_oid, expected_oid);

    // ...and a server-signed op record was emitted for it (the attested-push
    // path, not a bare local ref write).
    let op_log = refs
        .get(&RefName::new(git_protocol::attestation::OP_LOG_REF))
        .expect("read op log ref");
    assert!(op_log.is_some(), "PUT must emit an op record");
}

#[tokio::test(flavor = "multi_thread")]
async fn put_is_disabled_without_a_signing_key() {
    let repo = bare_repo();
    let config = Config {
        repo: repo.path().to_owned(),
        signing_key: None,
        token: None,
    };
    let (app, _counters) = router(config);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });

    let (status, _) = request(addr, "PUT", "/some-key", &[], b"bytes").await;
    assert_eq!(status, 405);
}
