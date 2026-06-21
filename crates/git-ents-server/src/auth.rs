//! Device-flow authentication: issue and validate access tokens.
//!
//! A client starts a login with `POST /auth/device`, shows the returned
//! `user_code` to the operator, and polls `POST /auth/token` until the request
//! is approved. Approval happens out of band: someone opens `GET /auth/verify`
//! and confirms the code. For now the only approver is the admin holding
//! `ACCESS_TOKEN`; OAuth backends (GitHub, atproto) slot in at the approval
//! step later, each minting a token bound to a richer [`Principal`].

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use axum::Json;
use axum::extract::{Form, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::AppState;

/// How long a pending login (and its `user_code`) stays valid.
const CODE_TTL: Duration = Duration::from_secs(900);
/// Seconds a polling client should wait between `POST /auth/token` calls.
const POLL_INTERVAL_SECS: u64 = 5;
/// Crockford-style alphabet for `user_code`s: no `0/O/1/I` ambiguity.
const USER_CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// Who an issued token belongs to. Stage (a) only mints [`Principal::Admin`];
/// the OAuth backends add the other variants.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id")]
pub(crate) enum Principal {
    Admin,
    Github(String),
    Atproto(String),
}

/// On-disk shape of the token store.
#[derive(Default, Serialize, Deserialize)]
struct TokenFile {
    tokens: HashMap<String, Principal>,
}

/// A login awaiting approval, keyed by its `device_code`.
struct PendingLogin {
    user_code: String,
    expires_at: SystemTime,
    /// Set once approved; the token the client's next poll will receive.
    token: Option<String>,
}

/// Result of a client polling for its token.
enum PollOutcome {
    Pending,
    Approved(String),
    Expired,
    Unknown,
}

/// Why an approval attempt failed.
#[derive(Debug)]
enum ApproveError {
    UnknownCode,
    Internal,
}

/// Issued tokens (persisted) plus in-flight logins (in-memory).
pub(crate) struct Auth {
    path: PathBuf,
    tokens: HashMap<String, Principal>,
    pending: HashMap<String, PendingLogin>,
}

impl Auth {
    /// Load previously issued tokens from `data_dir`, if any.
    pub(crate) fn load(data_dir: &Path) -> Self {
        let path = data_dir.join(".git-ents-tokens.json");
        let tokens = File::open(&path)
            .ok()
            .and_then(|file| serde_json::from_reader::<_, TokenFile>(file).ok())
            .map(|file| file.tokens)
            .unwrap_or_default();
        Self {
            path,
            tokens,
            pending: HashMap::new(),
        }
    }

    /// Whether no tokens have ever been issued (used to decide if the git
    /// endpoint should require auth at all).
    pub(crate) fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    /// Whether `token` is a currently issued access token.
    pub(crate) fn validate(&self, token: &str) -> bool {
        self.tokens.contains_key(token)
    }

    /// Begin a login, returning `(device_code, user_code)`.
    fn start(&mut self) -> Option<(String, String)> {
        let device_code = random_hex()?;
        let user_code = random_user_code()?;
        let expires_at = SystemTime::now().checked_add(CODE_TTL)?;
        self.pending.insert(
            device_code.clone(),
            PendingLogin {
                user_code: user_code.clone(),
                expires_at,
                token: None,
            },
        );
        Some((device_code, user_code))
    }

    /// Check on a login; consumes the pending entry once it resolves.
    fn poll(&mut self, device_code: &str) -> PollOutcome {
        match self.pending.get(device_code) {
            None => PollOutcome::Unknown,
            Some(pending) if SystemTime::now() >= pending.expires_at => {
                self.pending.remove(device_code);
                PollOutcome::Expired
            }
            Some(pending) => match pending.token.clone() {
                Some(token) => {
                    self.pending.remove(device_code);
                    PollOutcome::Approved(token)
                }
                None => PollOutcome::Pending,
            },
        }
    }

    /// Approve the login carrying `user_code`, minting a token for `principal`.
    fn approve(&mut self, user_code: &str, principal: Principal) -> Result<(), ApproveError> {
        let wanted = normalize_user_code(user_code);
        let now = SystemTime::now();
        let device_code = self
            .pending
            .iter()
            .find(|(_, pending)| {
                normalize_user_code(&pending.user_code) == wanted && now < pending.expires_at
            })
            .map(|(code, _)| code.clone())
            .ok_or(ApproveError::UnknownCode)?;

        let token = random_hex().ok_or(ApproveError::Internal)?;
        self.tokens.insert(token.clone(), principal);
        if let Some(pending) = self.pending.get_mut(&device_code) {
            pending.token = Some(token);
        }
        self.persist();
        Ok(())
    }

    /// Write the issued tokens to disk, logging (but not failing on) I/O errors.
    fn persist(&self) {
        if let Some(parent) = self.path.parent() {
            let _created = std::fs::create_dir_all(parent);
        }
        match File::create(&self.path) {
            Ok(file) => {
                let snapshot = TokenFile {
                    tokens: self.tokens.clone(),
                };
                if let Err(e) = serde_json::to_writer(file, &snapshot) {
                    eprintln!("error: failed to write token store: {e}");
                }
            }
            Err(e) => eprintln!("error: failed to open token store: {e}"),
        }
    }
}

/// `POST /auth/device` — mint a `device_code`/`user_code` pair to display.
pub(crate) async fn device(State(state): State<AppState>) -> Response {
    let started = match state.auth.lock() {
        Ok(mut guard) => guard.start(),
        Err(e) => return internal(&format!("auth lock poisoned: {e}")),
    };
    let Some((device_code, user_code)) = started else {
        return internal("failed to generate login codes");
    };
    Json(DeviceResponse {
        device_code,
        user_code,
        verification_uri: format!("{}/auth/verify", state.public_url),
        expires_in: CODE_TTL.as_secs(),
        interval: POLL_INTERVAL_SECS,
    })
    .into_response()
}

/// `POST /auth/token` — a client polling for its access token.
pub(crate) async fn token(
    State(state): State<AppState>,
    Json(request): Json<TokenRequest>,
) -> Response {
    let outcome = match state.auth.lock() {
        Ok(mut guard) => guard.poll(&request.device_code),
        Err(e) => return internal(&format!("auth lock poisoned: {e}")),
    };
    match outcome {
        PollOutcome::Approved(access_token) => {
            Json(TokenResponse { access_token }).into_response()
        }
        PollOutcome::Pending => oauth_error(StatusCode::BAD_REQUEST, "authorization_pending"),
        PollOutcome::Expired => oauth_error(StatusCode::BAD_REQUEST, "expired_token"),
        PollOutcome::Unknown => oauth_error(StatusCode::BAD_REQUEST, "invalid_grant"),
    }
}

/// `GET /auth/verify` — the page where an operator confirms a `user_code`.
pub(crate) async fn verify_page() -> Html<&'static str> {
    Html(VERIFY_FORM)
}

/// `POST /auth/verify` — approve a login. Stage (a): admin token only.
pub(crate) async fn verify_submit(
    State(state): State<AppState>,
    Form(form): Form<VerifyForm>,
) -> Response {
    let Some(admin) = state.access_token.as_deref() else {
        return (
            StatusCode::FORBIDDEN,
            Html("admin approval is not configured (no ACCESS_TOKEN set)"),
        )
            .into_response();
    };
    if form.admin_token != admin {
        return (StatusCode::UNAUTHORIZED, Html("invalid admin token")).into_response();
    }

    let result = match state.auth.lock() {
        Ok(mut guard) => guard.approve(&form.user_code, Principal::Admin),
        Err(e) => return internal(&format!("auth lock poisoned: {e}")),
    };
    match result {
        Ok(()) => Html(APPROVED_PAGE).into_response(),
        Err(ApproveError::UnknownCode) => {
            (StatusCode::BAD_REQUEST, Html("unknown or expired code")).into_response()
        }
        Err(ApproveError::Internal) => internal("failed to mint token"),
    }
}

/// `POST /auth/device` response.
#[derive(Serialize)]
struct DeviceResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: u64,
}

/// `POST /auth/token` request body.
#[derive(Deserialize)]
pub(crate) struct TokenRequest {
    device_code: String,
}

/// `POST /auth/token` success body.
#[derive(Serialize)]
struct TokenResponse {
    access_token: String,
}

/// `POST /auth/verify` form fields.
#[derive(Deserialize)]
pub(crate) struct VerifyForm {
    user_code: String,
    admin_token: String,
}

/// 32 random bytes rendered as 64 hex chars; used for device codes and tokens.
fn random_hex() -> Option<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).ok()?;
    let mut out = String::with_capacity(64);
    for byte in bytes {
        write!(out, "{byte:02x}").ok()?;
    }
    Some(out)
}

/// A human-friendly `XXXX-XXXX` code drawn from [`USER_CODE_ALPHABET`].
fn random_user_code() -> Option<String> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).ok()?;
    let mut code = String::with_capacity(9);
    for (index, byte) in bytes.iter().enumerate() {
        if index == 4 {
            code.push('-');
        }
        let letter = USER_CODE_ALPHABET.get((byte & 0x1f) as usize).copied()?;
        code.push(letter as char);
    }
    Some(code)
}

/// Collapse a `user_code` to its comparable form: uppercase, alphanumerics only.
fn normalize_user_code(input: &str) -> String {
    input
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

/// An OAuth-style `{ "error": "..." }` body with the given status.
fn oauth_error(status: StatusCode, code: &str) -> Response {
    (status, Json(serde_json::json!({ "error": code }))).into_response()
}

/// A `500` carrying a short message, logged server-side.
fn internal(message: &str) -> Response {
    eprintln!("error: {message}");
    (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
}

const VERIFY_FORM: &str = r#"<!doctype html>
<title>git-ents — approve a login</title>
<h1>Approve a device login</h1>
<form method="post" action="/auth/verify">
  <p><label>Code: <input name="user_code" placeholder="XXXX-XXXX" autofocus></label></p>
  <p><label>Admin token: <input name="admin_token" type="password"></label></p>
  <p><button type="submit">Approve</button></p>
</form>
"#;

const APPROVED_PAGE: &str = r#"<!doctype html>
<title>git-ents — approved</title>
<h1>Approved</h1>
<p>You can return to your terminal.</p>
"#;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "unit tests may unwrap freely")]

    use super::*;

    #[test]
    fn device_flow_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut auth = Auth::load(dir.path());
        assert!(auth.is_empty());

        let (device_code, user_code) = auth.start().unwrap();
        assert!(matches!(auth.poll(&device_code), PollOutcome::Pending));

        auth.approve(&user_code, Principal::Admin).unwrap();

        let PollOutcome::Approved(token) = auth.poll(&device_code) else {
            panic!("login should be approved");
        };
        assert!(auth.validate(&token));
        assert!(!auth.is_empty());
        // The pending entry is consumed once its token is collected.
        assert!(matches!(auth.poll(&device_code), PollOutcome::Unknown));
    }

    #[test]
    fn approval_accepts_dashless_lowercase_codes() {
        let dir = tempfile::tempdir().unwrap();
        let mut auth = Auth::load(dir.path());
        let (_device, user_code) = auth.start().unwrap();

        let messy = user_code.replace('-', "").to_lowercase();
        auth.approve(&messy, Principal::Admin).unwrap();
    }

    #[test]
    fn approval_rejects_unknown_code() {
        let dir = tempfile::tempdir().unwrap();
        let mut auth = Auth::load(dir.path());
        assert!(matches!(
            auth.approve("ZZZZ-ZZZZ", Principal::Admin),
            Err(ApproveError::UnknownCode)
        ));
    }

    #[test]
    fn issued_tokens_survive_reload() {
        let dir = tempfile::tempdir().unwrap();
        let token = {
            let mut auth = Auth::load(dir.path());
            let (device_code, user_code) = auth.start().unwrap();
            auth.approve(&user_code, Principal::Github("alice".to_owned()))
                .unwrap();
            match auth.poll(&device_code) {
                PollOutcome::Approved(token) => token,
                _ => panic!("login should be approved"),
            }
        };

        let reloaded = Auth::load(dir.path());
        assert!(reloaded.validate(&token));
    }
}
