//! Smart-HTTP through the native `git-protocol` traits (WS3), mounted
//! additively under `/_native/` alongside the existing `git http-backend`
//! CGI gateway (`crate::http`).
//!
//! `docs/scale-out.adoc`'s "Protocol traits" section explicitly permits more
//! than one conforming implementation behind `Advertise`/`Negotiate`/
//! `GeneratePack`/`IngestPack` — "whether that beats the native
//! implementation is empirical, settled by conformance plus cost, not by
//! fiat." `crate::http`'s CGI gateway already *is* the stock-git-wrapped
//! backend the plan describes as WS0, shipped first and load-bearing (hooks,
//! the checks queue, signed-push nonces); replacing it outright to satisfy
//! WS3 would be the larger, riskier change for no correctness gain over
//! mounting the native path beside it. This module is that native path,
//! wired end-to-end: `GET .../info/refs` and `POST .../git-upload-pack`
//! serve a stock `git clone`/`fetch` with zero client configuration, and
//! `POST .../git-receive-pack` ingests real pushes — including a stock
//! `git push --signed` (`push.gpgSign`) — through
//! [`git_protocol::IngestPack`], the same staged-then-atomic-then-promoted
//! ordering and attestation check the unit tests in `git-protocol`
//! exercise directly.
//!
//! The push-certificate wire protocol: the receive-pack advertisement
//! carries a `push-cert=<nonce>` capability; a signing client answers with
//! a `push-cert` pkt-line block in place of the plain command list —
//! certificate header (echoing the nonce), the commands themselves, and an
//! SSH signature — which [`parse_receive_request`] reassembles into the
//! exact payload the client signed. The nonce is session-scoped
//! anti-replay, never durable state (`docs/scale-out.adoc`, "Attested
//! push"): it is a keyed hash of a per-process secret, the repository, and
//! a timestamp, verified by recomputation within a slop window rather than
//! by storing anything.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use git_backend::{Expected, PackStream, RefEdit, RefName};
use git_protocol::native::{BackendResolver, NativeBackend, RepoBackends};
use git_protocol::{
    AdSpec, Advertise as _, GeneratePack as _, IngestPack as _, Negotiate as _, NegotiationState,
    PushCertificate, PushRequest, RepoId,
};
use gix_hash::ObjectId;

use crate::AppState;

/// Resolves a [`RepoId`] to `refstore-files`/`odb-files` backends opened
/// against `data_dir.join(repo)`, and to that repository's currently
/// enrolled members/config — loaded fresh per call, exactly what
/// `pre-receive` does, so both write paths see the identical trust set.
struct DiskResolver {
    data_dir: PathBuf,
}

impl BackendResolver for DiskResolver {
    fn resolve(&self, repo: &RepoId) -> git_protocol::Result<RepoBackends> {
        let path = self.data_dir.join(repo.as_str());
        let refs = refstore_files::FilesRefStore::open(&path)
            .map_err(|error| git_protocol::Error::UnknownRepo(error.to_string()))?;
        let objects = odb_files::OdbFiles::open(&path)
            .map_err(|error| git_protocol::Error::UnknownRepo(error.to_string()))?;
        let members = git_member::members::load_all(&path)
            .map_err(|error| git_protocol::Error::UnknownRepo(error.to_string()))?;
        let revoked = git_member::revocations::fingerprints(&path)
            .map_err(|error| git_protocol::Error::UnknownRepo(error.to_string()))?;
        let config = git_ents_core::config::load(&path)
            .map_err(|error| git_protocol::Error::UnknownRepo(error.to_string()))?;
        Ok(RepoBackends {
            refs: Arc::new(refs),
            objects: Arc::new(objects),
            authorized_members: git_member::members::without_revoked(members, &revoked),
            config,
            // No reachability artifacts for the disk-hydrated backend yet:
            // `gix-reachability`'s maintenance effect and artifact storage
            // target the cloud stack (`odb-tigris` + its pack registry);
            // wiring generation/loading for this resolver is future work,
            // and negotiation/ingest degrade to the plain walk in the
            // meantime (`docs/scale-out.adoc`: "absence ... degrades
            // speed, never answers").
            reachability: gix_reachability::ArtifactBundle::empty(),
        })
    }
}

fn backend(state: &AppState) -> NativeBackend<DiskResolver> {
    let signer: Arc<dyn git_protocol::attestation::OpSigner> = match &state.web_signing_key {
        Some(key) => Arc::new(git_protocol::attestation::SshOpSigner::new(key.clone())),
        // No server signing key configured: op records fail to sign, so
        // every otherwise-acceptable push over this endpoint is rejected —
        // fail-closed, since an accepted push without its op record would
        // break the "universal server op record" rule. Reads are
        // unaffected.
        None => Arc::new(git_protocol::attestation::SshOpSigner::new(PathBuf::from(
            "/dev/null",
        ))),
    };
    NativeBackend::new(
        DiskResolver {
            data_dir: state.data_dir.clone(),
        },
        signer,
    )
}

/// Serve `GET /_native/<repo>/info/refs?service=<git-upload-pack|git-receive-pack>`.
pub async fn get_request(State(state): State<AppState>, uri: Uri) -> Response {
    let Some((repo_rel, "info/refs")) = split(uri.path()) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let query = uri.query().unwrap_or_default();
    let service = crate::http::query_service(query).unwrap_or("git-upload-pack");
    if service != "git-upload-pack" && service != "git-receive-pack" {
        return (StatusCode::BAD_REQUEST, "unknown service").into_response();
    }
    let receive = service == "git-receive-pack";

    let repo_path = state.data_dir.join(repo_rel);
    if receive {
        if let Err(response) = crate::http::ensure_repo(&state, &repo_path).await {
            return response;
        }
    } else if !crate::http::is_bare_repo(&repo_path) {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }

    let ad = match backend(&state).refs(&RepoId::new(repo_rel), &AdSpec::everything()) {
        Ok(ad) => ad,
        Err(error) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response();
        }
    };

    let nonce = if receive { issue_nonce(repo_rel) } else { None };
    let mut body = pkt_line(format!("# service={service}\n").as_bytes());
    body.extend_from_slice(FLUSH_PKT);
    body.extend(advertisement_lines(&ad, receive, nonce.as_deref()));

    Response::builder()
        .header(
            "Content-Type",
            format!("application/x-{service}-advertisement"),
        )
        .header("Cache-Control", "no-cache")
        .body(axum::body::Body::from(body))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// Serve `POST /_native/<repo>/git-upload-pack` or `.../git-receive-pack`.
pub async fn post_request(
    State(state): State<AppState>,
    uri: Uri,
    _headers: HeaderMap,
    body: Bytes,
) -> Response {
    match split(uri.path()) {
        Some((repo_rel, "git-upload-pack")) => upload_pack(&state, repo_rel, &body).await,
        Some((repo_rel, "git-receive-pack")) => receive_pack(&state, repo_rel, &body).await,
        _ => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

async fn upload_pack(state: &AppState, repo_rel: &str, body: &[u8]) -> Response {
    let (wants, haves) = parse_upload_request(body);
    let backend = backend(state);
    let mut session = NegotiationState {
        repo: RepoId::new(repo_rel),
        wants,
        haves,
    };
    let plan = match backend.wants_haves(&mut session) {
        Ok(plan) => plan,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, error.to_string()).into_response();
        }
    };
    let mut stream = match backend.stream(&plan) {
        Ok(stream) => stream,
        Err(error) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response();
        }
    };
    let mut pack_bytes = Vec::new();
    if let Err(error) = std::io::Read::read_to_end(&mut stream, &mut pack_bytes) {
        return (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response();
    }

    let mut out = pkt_line(b"NAK\n");
    out.extend(pack_bytes);
    Response::builder()
        .header("Content-Type", "application/x-git-upload-pack-result")
        .body(axum::body::Body::from(out))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

async fn receive_pack(state: &AppState, repo_rel: &str, body: &[u8]) -> Response {
    let (commands, pack) = split_commands(body);
    let parsed = parse_receive_request(&commands);
    let names: Vec<RefName> = parsed
        .ref_edits
        .iter()
        .map(|edit| edit.name.clone())
        .collect();

    // The nonce echo check — transport-level anti-replay, ahead of the
    // signature/authorization checks IngestPack::receive itself makes. A
    // certificate whose nonce is not one this process recently issued for
    // this repository is a replayed (or cross-repo) certificate, rejected
    // before anything is staged.
    if parsed.cert.is_some() {
        let nonce_ok = parsed
            .nonce
            .as_deref()
            .is_some_and(|nonce| nonce_valid(repo_rel, nonce));
        if !nonce_ok {
            return report_status(
                &names,
                &Ok(git_protocol::PushOutcome::Rejected {
                    reason: "push certificate nonce was missing or stale".to_owned(),
                }),
            );
        }
    }

    let push = PushRequest {
        repo: RepoId::new(repo_rel),
        ref_edits: parsed.ref_edits,
        pack: PackStream::new(std::io::Cursor::new(pack.to_vec())),
        push_cert: parsed.cert.map(PushCertificate::new),
    };
    let outcome = backend(state).receive(push);

    // WS9's wired call site: an accepted push reports its ref-update
    // volume to the maintenance scheduler, which enqueues the maintenance
    // effects once the repo crosses its threshold (`docs/scale-out.adoc`,
    // "Reachability" / WS9). Off the request path (`spawn_blocking` — the
    // sink opens a Postgres connection when the threshold trips) and
    // never able to fail the push: scheduling errors are logged, the next
    // accepted push re-triggers.
    if matches!(outcome, Ok(git_protocol::PushOutcome::Accepted { .. }))
        && let Some(scheduler) = state.maintenance.clone()
    {
        let repo_id = repo_rel.to_owned();
        let updates = names.len() as u64;
        drop(tokio::task::spawn_blocking(move || {
            if let Err(error) = scheduler.note_ref_updates(&repo_id, updates) {
                eprintln!("maintenance: could not schedule for {repo_id}: {error}");
            }
        }));
    }

    report_status(&names, &outcome)
}

/// The report-status response for one push: `unpack ok`, then one `ok`/`ng`
/// line per ref.
fn report_status(
    names: &[RefName],
    outcome: &git_protocol::Result<git_protocol::PushOutcome>,
) -> Response {
    let mut out = pkt_line(b"unpack ok\n");
    match outcome {
        Ok(git_protocol::PushOutcome::Accepted { .. }) => {
            for name in names {
                out.extend(pkt_line(format!("ok {name}\n").as_bytes()));
            }
        }
        Ok(git_protocol::PushOutcome::Rejected { reason }) => {
            for name in names {
                out.extend(pkt_line(format!("ng {name} {reason}\n").as_bytes()));
            }
        }
        Err(error) => {
            for name in names {
                out.extend(pkt_line(format!("ng {name} {error}\n").as_bytes()));
            }
        }
    }
    out.extend_from_slice(FLUSH_PKT);
    Response::builder()
        .header("Content-Type", "application/x-git-receive-pack-result")
        .body(axum::body::Body::from(out))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// Split `/_native/<repo>/<suffix>` into `(repo, suffix)` for `suffix` in
/// `{"info/refs", "git-upload-pack", "git-receive-pack"}`, validating every
/// repo path segment exactly as the CGI gateway does.
fn split(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix("/_native/")?;
    for suffix in ["info/refs", "git-upload-pack", "git-receive-pack"] {
        if let Some(repo) = rest.strip_suffix(suffix) {
            let repo = repo.strip_suffix('/')?;
            let segments: Vec<&str> = repo.split('/').filter(|s| !s.is_empty()).collect();
            if segments.is_empty()
                || segments.len() > crate::http::MAX_REPO_DEPTH
                || !segments
                    .iter()
                    .all(|segment| crate::http::valid_segment(segment))
            {
                return None;
            }
            return Some((repo, suffix));
        }
    }
    None
}

const FLUSH_PKT: &[u8] = b"0000";

fn pkt_line(data: &[u8]) -> Vec<u8> {
    let mut out = format!("{:04x}", data.len().saturating_add(4)).into_bytes();
    out.extend_from_slice(data);
    out
}

/// Every pkt-line in `body`, in order, skipping flush markers — correct for
/// `git-upload-pack`'s request, which is pkt-lines from start to end with no
/// trailing binary payload.
fn pkt_lines(mut body: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    while body.len() >= 4 {
        let Ok(len_str) = std::str::from_utf8(body.get(..4).unwrap_or_default()) else {
            break;
        };
        let Ok(len) = usize::from_str_radix(len_str, 16) else {
            break;
        };
        if len == 0 {
            body = body.get(4..).unwrap_or_default();
            continue;
        }
        if len < 4 || body.len() < len {
            break;
        }
        out.push(body.get(4..len).unwrap_or_default().to_vec());
        body = body.get(len..).unwrap_or_default();
    }
    out
}

/// Pkt-line commands up to (and past) the first flush, and the raw bytes
/// remaining after it — `git-receive-pack`'s request is pkt-line commands
/// followed by a flush, then the pack as an unframed byte stream.
fn split_commands(body: &[u8]) -> (Vec<Vec<u8>>, &[u8]) {
    let mut commands = Vec::new();
    let mut offset = 0usize;
    while offset.saturating_add(4) <= body.len() {
        let Some(len_hex) = body.get(offset..offset.saturating_add(4)) else {
            break;
        };
        let Ok(len_str) = std::str::from_utf8(len_hex) else {
            break;
        };
        let Ok(len) = usize::from_str_radix(len_str, 16) else {
            break;
        };
        if len == 0 {
            offset = offset.saturating_add(4);
            break;
        }
        if len < 4 {
            break;
        }
        let Some(end) = offset.checked_add(len).filter(|end| *end <= body.len()) else {
            break;
        };
        commands.push(
            body.get(offset.saturating_add(4)..end)
                .unwrap_or_default()
                .to_vec(),
        );
        offset = end;
    }
    (commands, body.get(offset..).unwrap_or_default())
}

fn parse_upload_request(body: &[u8]) -> (Vec<ObjectId>, Vec<ObjectId>) {
    let mut wants = Vec::new();
    let mut haves = Vec::new();
    for line in pkt_lines(body) {
        let text = String::from_utf8_lossy(&line);
        let text = text.trim_end_matches('\n');
        if let Some(rest) = text.strip_prefix("want ") {
            if let Some(hex) = rest.split_whitespace().next()
                && let Ok(oid) = ObjectId::from_hex(hex.as_bytes())
            {
                wants.push(oid);
            }
        } else if let Some(hex) = text.strip_prefix("have ")
            && let Ok(oid) = ObjectId::from_hex(hex.as_bytes())
        {
            haves.push(oid);
        }
    }
    (wants, haves)
}

fn parse_command(line: &[u8]) -> Option<RefEdit> {
    let text = String::from_utf8_lossy(line);
    let text = text.trim_end_matches('\n');
    // The first command carries a NUL-separated capability list.
    let text = text.split('\0').next().unwrap_or(text);
    let mut parts = text.split_whitespace();
    let old_hex = parts.next()?;
    let new_hex = parts.next()?;
    let name = parts.next()?;
    let old = ObjectId::from_hex(old_hex.as_bytes()).ok()?;
    let new = ObjectId::from_hex(new_hex.as_bytes()).ok()?;
    let null = ObjectId::null(gix_hash::Kind::Sha1);
    Some(RefEdit {
        name: RefName::new(name),
        expected: if old == null {
            Expected::MustNotExist
        } else {
            Expected::MustExistAndMatch(old)
        },
        new: (new != null).then_some(new),
    })
}

/// A parsed receive-pack request: the ref edits it asks for, plus — when
/// the client answered the `push-cert` capability — the reassembled
/// certificate text (exactly the bytes the client signed, then its
/// signature) and the nonce it echoed.
struct ParsedReceive {
    ref_edits: Vec<RefEdit>,
    cert: Option<String>,
    nonce: Option<String>,
}

/// Parse a receive-pack request's pkt-lines: either a plain command list,
/// or a `push-cert` block (`pack-protocol.txt`: a `push-cert` line carrying
/// the client's capabilities, the certificate header ending at a blank
/// line, the command lines, the SSH signature block, then
/// `push-cert-end`). With a certificate, the commands inside it are the
/// authoritative ones — a client using `push-cert` sends no plain command
/// list at all.
fn parse_receive_request(commands: &[Vec<u8>]) -> ParsedReceive {
    let first_is_cert = commands.first().is_some_and(|line| {
        String::from_utf8_lossy(line)
            .split('\0')
            .next()
            .unwrap_or_default()
            .trim_end()
            == "push-cert"
    });
    if !first_is_cert {
        return ParsedReceive {
            ref_edits: commands
                .iter()
                .filter_map(|line| parse_command(line))
                .collect(),
            cert: None,
            nonce: None,
        };
    }

    let mut cert = String::new();
    let mut nonce = None;
    let mut ref_edits = Vec::new();
    let mut past_header = false;
    let mut in_signature = false;
    for line in commands.iter().skip(1) {
        let text = String::from_utf8_lossy(line);
        if text.trim_end() == "push-cert-end" {
            break;
        }
        // Certificate pkt-lines carry their own LF; concatenating them
        // as-is reconstructs the exact payload the client signed.
        cert.push_str(&text);
        let trimmed = text.trim_end_matches('\n');
        if !past_header {
            if let Some(value) = trimmed.strip_prefix("nonce ") {
                nonce = Some(value.to_owned());
            }
            if trimmed.is_empty() {
                past_header = true;
            }
            continue;
        }
        if trimmed.starts_with("-----BEGIN") {
            in_signature = true;
        }
        if !in_signature && let Some(edit) = parse_command(line) {
            ref_edits.push(edit);
        }
    }
    ParsedReceive {
        ref_edits,
        cert: Some(cert),
        nonce,
    }
}

/// How long an issued nonce stays echoable. The advertisement and the push
/// are two HTTP requests seconds apart; anything older is a replay. The
/// nonce is session-scoped anti-replay only, never durable state.
const NONCE_SLOP_SECONDS: u64 = 300;

/// The per-process secret nonces are keyed with. Process-scoped on
/// purpose: a nonce must survive exactly the advertisement→push window
/// within one server process, nothing longer.
fn nonce_secret() -> &'static str {
    static SECRET: OnceLock<String> = OnceLock::new();
    SECRET.get_or_init(|| uuid::Uuid::new_v4().to_string())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

/// The keyed hash binding a nonce to this process, `repo`, and `stamp`.
/// The secret is prepended, so recomputing it requires holding the secret.
fn nonce_mac(repo: &str, stamp: u64) -> Option<String> {
    let data = format!("{}:{repo}:{stamp}", nonce_secret());
    gix_object::compute_hash(
        gix_hash::Kind::Sha1,
        gix_object::Kind::Blob,
        data.as_bytes(),
    )
    .ok()
    .map(|oid| oid.to_hex().to_string())
}

/// A fresh nonce for `repo`, advertised as `push-cert=<nonce>` and expected
/// back in the certificate's `nonce` header.
fn issue_nonce(repo: &str) -> Option<String> {
    let stamp = unix_now();
    nonce_mac(repo, stamp).map(|mac| format!("{stamp}-{mac}"))
}

/// Whether `nonce` is one this process issued for `repo` within the slop
/// window — verified by recomputation, storing nothing.
fn nonce_valid(repo: &str, nonce: &str) -> bool {
    let Some((stamp, mac)) = nonce.split_once('-') else {
        return false;
    };
    let Ok(stamp) = stamp.parse::<u64>() else {
        return false;
    };
    nonce_mac(repo, stamp).as_deref() == Some(mac)
        && unix_now().saturating_sub(stamp) <= NONCE_SLOP_SECONDS
}

/// The advertised ref lines: `HEAD` first (carrying capabilities and, when
/// resolved, a `symref=HEAD:<name>` hint so `git clone` knows its default
/// branch) if resolved, then every other ref.
fn advertisement_lines(
    ad: &git_protocol::RefAdvertisement,
    receive: bool,
    push_cert_nonce: Option<&str>,
) -> Vec<u8> {
    let mut caps = if receive {
        "report-status delete-refs ofs-delta agent=git-ents/1.0".to_owned()
    } else {
        "ofs-delta agent=git-ents/1.0".to_owned()
    };
    if let Some(nonce) = push_cert_nonce {
        caps = format!("{caps} push-cert={nonce}");
    }
    if let Some(head) = &ad.head {
        caps = format!("{caps} symref=HEAD:{head}");
    }

    let mut out = Vec::new();
    if ad.refs.is_empty() {
        let null = ObjectId::null(gix_hash::Kind::Sha1);
        out.extend(pkt_line(
            format!("{null} capabilities^{{}}\0{caps}\n").as_bytes(),
        ));
        out.extend_from_slice(FLUSH_PKT);
        return out;
    }

    let head_oid = ad
        .head
        .as_ref()
        .and_then(|name| ad.refs.iter().find(|(n, _)| n == name))
        .map(|(_, oid)| *oid);
    let mut first = true;
    if let Some(oid) = head_oid {
        out.extend(pkt_line(format!("{oid} HEAD\0{caps}\n").as_bytes()));
        first = false;
    }
    for (name, oid) in &ad.refs {
        let line = if first {
            first = false;
            format!("{oid} {name}\0{caps}\n")
        } else {
            format!("{oid} {name}\n")
        };
        out.extend(pkt_line(line.as_bytes()));
    }
    out.extend_from_slice(FLUSH_PKT);
    out
}
