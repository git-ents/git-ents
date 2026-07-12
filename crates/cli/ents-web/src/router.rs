//! Wiring every page into one [`axum::Router`], plus the session/CSRF
//! middleware every state-changing route runs behind
//! (`roots.web-session`).
//!
//! [`router`] builds the [`axum::Router`] alone, with no socket ever
//! bound -- this is what lets [`crate`]'s own tests (and, per
//! `roots.web-agnostic`, an in-process webview embedding) drive a request
//! through this crate's full stack via `tower::ServiceExt::oneshot`
//! without any network transport existing at all. [`bind`]/[`serve_on`]
//! split socket binding from serving so a caller (`git-ents`'s own `serve`
//! command) can read back the bound port before the server starts
//! blocking -- necessary for `--port 0` ("pick any free port") to be
//! useful at all.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use gix_object::{Find, Write};

use crate::assets;
use crate::pages;
use crate::session;
use crate::state::AppState;

/// Build the full route table, wrapped in the session middleware
/// (`roots.web-session`).
///
/// Nothing here binds a socket: this `Router` is a plain, in-process
/// `tower::Service` (`roots.web-agnostic`) -- see this module's own doc.
// @relation(roots.web-agnostic, roots.local, scope=function)
pub fn router<O>(state: Arc<AppState<O>>) -> Router
where
    O: Find + Write + Send + 'static,
{
    Router::new()
        .route("/", get(pages::dashboard::show::<O>))
        .route("/members", get(pages::members::list::<O>))
        .route("/members/{username}", get(pages::members::show::<O>))
        .route(
            "/account",
            get(pages::account::show::<O>).post(pages::account::update::<O>),
        )
        .route("/effects", get(pages::effects::list::<O>))
        .route("/effects/{name}", get(pages::effects::show::<O>))
        .route("/commits", get(pages::commits::list::<O>))
        .route("/commit/{oid}", get(pages::commits::show::<O>))
        .route("/files", get(pages::files::root::<O>))
        .route("/files/{*path}", get(pages::files::show::<O>))
        .route("/meta", get(pages::meta::show::<O>))
        .route("/redactions", get(pages::redactions::list::<O>))
        .route("/redactions/{id}", get(pages::redactions::show::<O>))
        .route("/search", get(pages::search::show::<O>))
        .route("/toolchains", get(pages::toolchains::list::<O>))
        .route("/toolchains/{name}", get(pages::toolchains::show::<O>))
        .route(
            "/comments",
            get(pages::comments::list::<O>).post(pages::comments::add::<O>),
        )
        .route("/comments/{id}", get(pages::comments::show::<O>))
        .route("/inbox", get(pages::inbox::list::<O>))
        .route("/style.css", get(style))
        .route("/ents.js", get(script))
        .layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            session_middleware::<O>,
        ))
        .with_state(state)
}

/// The session middleware (`roots.web-session`): recognize an existing
/// session cookie, or mint a fresh one and set it on the response. Every
/// handler reads the resolved [`session::Session`] via `Extension`.
// @relation(roots.web-session, scope=function)
async fn session_middleware<O>(
    State(state): State<Arc<AppState<O>>>,
    mut request: Request,
    next: Next,
) -> Response
where
    O: Find + Write + Send + 'static,
{
    let cookie_header = request
        .headers()
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let existing = cookie_header
        .as_deref()
        .and_then(session::session_id_from_cookie_header)
        .and_then(|id| {
            state
                .sessions
                .get(id)
                .map(|session| (id.to_owned(), session))
        });

    let (id, session, is_new) = match existing {
        Some((id, session)) => (id, session, false),
        None => {
            let (id, session) = state.sessions.create();
            (id, session, true)
        }
    };
    request.extensions_mut().insert(session);

    let mut response = next.run(request).await;
    if is_new && let Ok(value) = HeaderValue::from_str(&session::set_cookie_header(&id)) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    response
}

/// `GET /style.css`: the one stylesheet [`pages::layout`]'s `head` links --
/// the hand-rolled, ported pre-redo sheet (`crate::assets::OVERRIDES`). No
/// session or CSRF gating applies here (the session middleware only ever
/// attaches a session, never rejects a request), and it must not: every
/// page, including one reached before a session exists, needs this to
/// render styled.
async fn style() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        assets::OVERRIDES,
    )
}

/// `GET /ents.js`: the progressive-enhancement script
/// [`pages::layout`]'s `head` loads with `defer` (`crate::assets::SCRIPT`)
/// -- see [`crate::assets`]'s own doc for what it does. Served the same
/// way, and under the same no-session-gating rule, as [`style`].
async fn script() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        assets::SCRIPT,
    )
}

/// Bind a loopback-or-otherwise socket for [`serve_on`], returning the
/// listener before any request is served so a caller can read back
/// [`std::net::TcpListener::local_addr`] (necessary for `addr`'s port `0`,
/// "pick any free port," to be useful to a caller that must print or open
/// the resulting URL).
///
/// # Errors
///
/// Any [`std::io::Error`] binding the socket.
pub async fn bind(addr: SocketAddr) -> std::io::Result<tokio::net::TcpListener> {
    tokio::net::TcpListener::bind(addr).await
}

/// Serve `state`'s router on an already-bound `listener` until the process
/// is killed -- this crate has no shutdown signal of its own; a caller
/// that wants graceful shutdown wraps this future with one.
///
/// # Errors
///
/// Any [`std::io::Error`] the underlying accept loop hits.
pub async fn serve_on<O>(
    listener: tokio::net::TcpListener,
    state: Arc<AppState<O>>,
) -> std::io::Result<()>
where
    O: Find + Write + Send + 'static,
{
    axum::serve(listener, router(state)).await
}
