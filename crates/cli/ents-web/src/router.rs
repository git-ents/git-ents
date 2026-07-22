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
use axum::routing::{get, post};
use gix_object::{Find, Write};

use crate::assets;
use crate::pages;
use crate::session;
use crate::state::{AccessPolicy, AppState};

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
    // The sign-in surface exists only where the injected policy demands
    // it (`roots.web-signin`): under `Trusted` — the local root — these
    // routes are not merely inert, they are unrouted, so `/login` is a
    // plain 404 and the local surface is byte-identical to before.
    let sign_in = match state.access {
        AccessPolicy::SignInRequired(_) => Router::new()
            .route("/login", get(pages::login::show::<O>))
            .route(
                "/login/challenge/{code}",
                get(pages::login::challenge::<O>).post(pages::login::complete::<O>),
            )
            .route("/logout", post(pages::login::logout::<O>)),
        AccessPolicy::Trusted => Router::new(),
    };
    Router::new()
        .merge(sign_in)
        .route("/", get(pages::dashboard::show::<O>))
        .route("/members", get(pages::members::list::<O>))
        .route("/members/{username}", get(pages::members::show::<O>))
        .route(
            "/account",
            get(pages::account::show::<O>).post(pages::account::update::<O>),
        )
        .route(
            "/effects",
            get(pages::effects::list::<O>).post(pages::effects::create::<O>),
        )
        .route("/effects/{name}", get(pages::effects::show::<O>))
        .route("/commits", get(pages::commits::list::<O>))
        .route("/commit/{oid}", get(pages::commits::show::<O>))
        .route("/commit/{oid}/review", post(pages::commits::review::<O>))
        .route(
            "/reviews/{target}/{member}/comment",
            post(pages::commits::review_comment::<O>),
        )
        .route("/files", get(pages::files::root::<O>))
        .route("/files/{*path}", get(pages::files::show::<O>))
        .route("/meta", get(pages::meta::show::<O>))
        .route("/redactions", get(pages::redactions::list::<O>))
        .route("/redactions/{id}", get(pages::redactions::show::<O>))
        .route("/search", get(pages::search::show::<O>))
        .route(
            "/toolchains",
            get(pages::toolchains::list::<O>).post(pages::toolchains::register::<O>),
        )
        .route("/toolchains/{name}", get(pages::toolchains::show::<O>))
        .route(
            "/comments",
            get(pages::comments::list::<O>).post(pages::comments::add::<O>),
        )
        .route("/comments/{id}", get(pages::comments::show::<O>))
        .route("/comments/{id}/reply", post(pages::comments::reply::<O>))
        .route(
            "/comments/{id}/resolve",
            post(pages::comments::resolve::<O>),
        )
        .route("/comments/{id}/reopen", post(pages::comments::reopen::<O>))
        .route(
            "/issues",
            get(pages::issues::list::<O>).post(pages::issues::create::<O>),
        )
        .route(
            "/issues/{id}",
            get(pages::issues::show::<O>).post(pages::issues::edit::<O>),
        )
        .route("/issues/{id}/comment", post(pages::issues::comment::<O>))
        .route(
            "/agents",
            get(pages::agents::list::<O>).post(pages::agents::create::<O>),
        )
        .route("/agents/{id}", get(pages::agents::show::<O>))
        .route("/agents/{id}/confirm", post(pages::agents::confirm::<O>))
        .route("/agents/{id}/review", post(pages::agents::open_review::<O>))
        .route(
            "/agents/{id}/chat",
            get(pages::agent_chat::show::<O>).post(pages::agent_chat::send::<O>),
        )
        .route(
            "/agents/{id}/plan",
            post(pages::agent_chat::commit_plan::<O>),
        )
        .route("/agents/{id}/reopen", post(pages::agent_chat::reopen::<O>))
        .route("/inbox", get(pages::inbox::list::<O>))
        .route("/style.css", get(style))
        .route("/ents.js", get(script))
        .route("/fonts/{name}", get(font))
        // Layer order: axum runs the last-added layer first, so the
        // session middleware (added below) resolves the session before
        // the auth middleware consults it.
        .layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            auth_middleware::<O>,
        ))
        .layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            session_middleware::<O>,
        ))
        .with_state(state)
}

/// The access-policy middleware (`roots.web-signin`): under
/// [`AccessPolicy::Trusted`] every request passes untouched — the local
/// root's behavior is byte-identical to before this middleware existed.
/// Under [`AccessPolicy::SignInRequired`], a state-changing request (every
/// mutation in this crate is a `POST`) requires a session signed in as a
/// member who is *still* enrolled and active — re-checked here on every
/// mutation, so a revocation takes effect mid-session, not at the next
/// sign-in. `/login` and `/logout` are exempt: the sign-in surface itself
/// authenticates by signature, and logout only clears session state.
// @relation(roots.web-signin, scope=function)
async fn auth_middleware<O>(
    State(state): State<Arc<AppState<O>>>,
    request: Request,
    next: Next,
) -> Response
where
    O: Find + Write + Send + 'static,
{
    let AccessPolicy::SignInRequired(_) = &state.access else {
        return next.run(request).await;
    };
    let path = request.uri().path();
    if request.method() != axum::http::Method::POST
        || path.starts_with("/login")
        || path == "/logout"
    {
        return next.run(request).await;
    }

    let member = request
        .extensions()
        .get::<session::Session>()
        .and_then(|session| session.member.clone());
    let enrolled = match &member {
        Some(member) => crate::auth::active_member_by_key(&state, &member.key)
            .ok()
            .flatten()
            .is_some_and(|username| username == member.username),
        None => false,
    };
    if enrolled {
        return next.run(request).await;
    }

    // A signed-in member who no longer verifies against the live member
    // list is signed out, not just refused (`roots.web-signin`).
    if member.is_some()
        && let Some(id) = request
            .headers()
            .get(header::COOKIE)
            .and_then(|value| value.to_str().ok())
            .and_then(session::session_id_from_cookie_header)
    {
        state.sessions.clear_member(id);
    }

    let wants_html = request
        .headers()
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|accept| accept.contains("text/html"));
    if wants_html {
        axum::response::Redirect::to("/login").into_response()
    } else {
        (
            axum::http::StatusCode::UNAUTHORIZED,
            "sign in first: this deployment requires an authenticated member for mutations\n",
        )
            .into_response()
    }
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
    request
        .extensions_mut()
        .insert(session::SessionId(id.clone()));

    let mut response = next.run(request).await;
    // `Secure` is policy-driven: hosted (sign-in-required) serving is
    // HTTPS-only, local plain-HTTP loopback must not lose its cookie.
    let secure = matches!(state.access, AccessPolicy::SignInRequired(_));
    if is_new && let Ok(value) = HeaderValue::from_str(&session::set_cookie_header(&id, secure)) {
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

/// `GET /fonts/{name}`: one embedded IBM Plex woff2 face
/// (`crate::assets::font`), named by `ents.css`'s own `@font-face` `src`
/// URLs. Immutable and content-addressed by filename, so it carries a
/// year-long `immutable` cache; a name outside the fixed [`assets::FONTS`]
/// table is a plain 404, never a path escape. Served under the same
/// no-session-gating rule as [`style`] -- a face is needed to render every
/// page, including one reached before a session exists.
async fn font(axum::extract::Path(name): axum::extract::Path<String>) -> Response {
    match assets::font(&name) {
        Some(bytes) => (
            [
                (header::CONTENT_TYPE, "font/woff2"),
                (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
            ],
            bytes,
        )
            .into_response(),
        None => axum::http::StatusCode::NOT_FOUND.into_response(),
    }
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
