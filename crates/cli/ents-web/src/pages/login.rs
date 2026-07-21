//! `GET /login`, `GET`/`POST /login/challenge/{code}`, `POST /logout`:
//! the hosted sign-in surface (`roots.web-signin`), mounted only when the
//! composition root injected [`AccessPolicy::SignInRequired`] — a local
//! root has no sign-in surface at all, so under
//! [`AccessPolicy::Trusted`] these routes do not exist and `/login` is a
//! plain 404.
//!
//! The flow inverts a device-code login (`gh auth login`'s shape) because
//! here the *CLI* holds the credential: the browser's `/login` page mints
//! a short one-time code bound to its own session and displays the
//! `git ents login` command to run; the CLI fetches the full challenge
//! (`GET`, non-consuming), rebuilds the payload locally from the host the
//! member addressed, signs it under [`crate::auth::LOGIN_NAMESPACE`], and
//! posts the signature back (`POST`, consuming). The page refreshes
//! itself until the session reads as signed in.
//!
//! The CLI endpoints speak `key=value` text lines, not HTML forms'
//! escaping rules and not JSON — the same trivially-parseable shape the
//! challenge payload itself uses, so neither side grows a parser.

use std::sync::Arc;

use axum::Form;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use gix_object::{Find, Write};
use maud::html;
use serde::Deserialize;

use crate::auth;
use crate::session::{Session, SessionId, SessionMember};
use crate::state::{AccessPolicy, AppState, Realm};

/// The realm, or a 404 — these handlers are only ever routed under
/// [`AccessPolicy::SignInRequired`], so a miss here is a wiring error,
/// answered exactly as an unmounted route would be.
fn realm<O>(state: &AppState<O>) -> Result<&Realm, Box<Response>> {
    match &state.access {
        AccessPolicy::SignInRequired(realm) => Ok(realm),
        AccessPolicy::Trusted => Err(Box::new(StatusCode::NOT_FOUND.into_response())),
    }
}

/// `GET /login`: the sign-in page. Signed out, it mints a challenge
/// bound to this browser's session and shows the one command to run;
/// the page refreshes itself every few seconds until the session reads
/// as signed in — no script needed, and a challenge consumed by the CLI
/// re-issues on the next refresh only if sign-in did not complete.
// @relation(roots.web-signin, scope=function)
pub async fn show<O>(
    State(state): State<Arc<AppState<O>>>,
    axum::Extension(session): axum::Extension<Session>,
    axum::Extension(SessionId(session_id)): axum::Extension<SessionId>,
) -> Response
where
    O: Find + Write + Send + 'static,
{
    let realm = match realm(&state) {
        Ok(realm) => realm,
        Err(response) => return *response,
    };

    let body = match &session.member {
        Some(member) => html! {
            div.readable {
                p { "Signed in as " strong { (member.username) } "." }
                p.muted {
                    "Edits you make here are authored as this member and "
                    "signed by the server's own key -- history reads "
                    em { (member.username) " via the web" } "."
                }
                form method="post" action="/logout" {
                    input type="hidden" name="csrf" value=(session.csrf);
                    button.btn type="submit" { "Sign out" }
                }
            }
        },
        None => {
            let (code, _nonce) = realm.challenges.issue(&session_id);
            // The code is eight ASCII base32 characters by construction;
            // split_at is byte-indexed and cannot land inside a char.
            let (head, tail) = code.split_at(4);
            let display = format!("{head}-{tail}");
            html! {
                div.readable {
                    p {
                        "Prove control of an enrolled member key. Run this "
                        "on your own machine -- the key never leaves it:"
                    }
                    pre.login-code {
                        "git ents login https://" span.code { (realm.host) " " (display) }
                    }
                    p.muted {
                        "This page refreshes on its own; the code is "
                        "single-use and expires in ten minutes."
                    }
                }
            }
        }
    };

    let markup = super::layout(
        &super::RepoHeader::from_state(&state),
        &super::identity_label(&state),
        super::Tab::Account,
        "Sign in",
        body,
    );
    if session.member.is_some() {
        markup.into_response()
    } else {
        // A refresh header, not a script: the page re-renders as signed
        // in on the first refresh after the CLI completes the challenge.
        ([(header::HeaderName::from_static("refresh"), "3")], markup).into_response()
    }
}

/// `GET /login/challenge/{code}`: the CLI's fetch — the challenge's
/// bound facts as `key=value` lines, without consuming it. The CLI MUST
/// rebuild the payload from the host it addressed rather than trusting
/// these lines (`roots.web-signin`); they exist so it can carry the
/// nonce and confirm the host matches.
// @relation(roots.web-signin, scope=function)
pub async fn challenge<O>(
    State(state): State<Arc<AppState<O>>>,
    Path(code): Path<String>,
) -> Response
where
    O: Find + Write + Send + 'static,
{
    let realm = match realm(&state) {
        Ok(realm) => realm,
        Err(response) => return *response,
    };
    match realm.challenges.peek(&code) {
        Some(challenge) => (
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            format!(
                "host={}\ncode={}\nnonce={}\n",
                realm.host,
                auth::normalize_code(&code),
                challenge.nonce
            ),
        )
            .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            "unknown or expired code; reload the sign-in page for a fresh one\n",
        )
            .into_response(),
    }
}

/// What the CLI posts back to complete a sign-in: the member's public
/// key line and the armored SSHSIG over the locally-rebuilt payload.
#[derive(Deserialize)]
pub struct Completion {
    /// The member's OpenSSH public key line.
    pub public_key: String,
    /// The armored SSHSIG PEM over [`crate::auth::challenge_payload`].
    pub signature: String,
}

/// `POST /login/challenge/{code}`: consume the challenge, verify the
/// signature over the server's own reconstruction of the payload, check
/// the key names an enrolled *active* member, and mark the bound browser
/// session signed in. Deliberately outside the session/CSRF discipline:
/// this request carries no cookie and authenticates by signature alone
/// (`roots.web-signin`).
// @relation(roots.web-signin, scope=function)
pub async fn complete<O>(
    State(state): State<Arc<AppState<O>>>,
    Path(code): Path<String>,
    Form(completion): Form<Completion>,
) -> Response
where
    O: Find + Write + Send + 'static,
{
    let realm = match realm(&state) {
        Ok(realm) => realm,
        Err(response) => return *response,
    };
    let Some(challenge) = realm.challenges.take(&code) else {
        return (
            StatusCode::NOT_FOUND,
            "unknown or expired code; reload the sign-in page for a fresh one\n",
        )
            .into_response();
    };

    let code = auth::normalize_code(&code);
    let payload = auth::challenge_payload(&realm.host, &code, &challenge.nonce);
    let public_key = completion.public_key.trim();
    if !auth::verify_login(public_key, payload.as_bytes(), &completion.signature) {
        return (
            StatusCode::UNAUTHORIZED,
            "the signature did not verify against that key for this host and code\n",
        )
            .into_response();
    }
    let username = match auth::active_member_by_key(&state, public_key) {
        Ok(Some(username)) => username,
        Ok(None) => {
            return (
                StatusCode::UNAUTHORIZED,
                "that key is not an enrolled, active member of this repository\n",
            )
                .into_response();
        }
        Err(error) => return error.into_response(),
    };

    let member = SessionMember {
        username: username.clone(),
        key: public_key.to_owned(),
    };
    if !state.sessions.authenticate(&challenge.session_id, member) {
        return (
            StatusCode::GONE,
            "the browser session that requested this code no longer exists; reload the sign-in \
             page\n",
        )
            .into_response();
    }
    (
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        format!("member={username}\n"),
    )
        .into_response()
}

/// What the logout form posts: the session's CSRF token.
#[derive(Deserialize)]
pub struct LogoutForm {
    /// The per-session CSRF token (`roots.web-session`).
    pub csrf: String,
}

/// `POST /logout`: drop the session's signed-in member, keeping the
/// session (and its CSRF token) itself. CSRF-checked like every other
/// browser mutation — a cross-site form must not be able to sign the
/// user out.
// @relation(roots.web-signin, roots.web-session, scope=function)
pub async fn logout<O>(
    State(state): State<Arc<AppState<O>>>,
    axum::Extension(session): axum::Extension<Session>,
    axum::Extension(SessionId(session_id)): axum::Extension<SessionId>,
    Form(form): Form<LogoutForm>,
) -> crate::Result<impl IntoResponse>
where
    O: Find + Write + Send + 'static,
{
    super::require_csrf(&session, &form.csrf)?;
    state.sessions.clear_member(&session_id);
    Ok(Redirect::to("/"))
}
