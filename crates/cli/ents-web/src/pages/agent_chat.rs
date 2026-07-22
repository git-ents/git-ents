//! `GET /agents/{id}/chat`, `POST /agents/{id}/chat`, `POST
//! /agents/{id}/plan`, `POST /agents/{id}/reopen`: the laptop
//! planning-chat page (`docs/agent-sessions-plan.adoc`'s Phase 4) — linked
//! from `crate::pages::agents::show` whenever a session is `planning` or
//! `ready`.
//!
//! Every read is `ents_forge::agent::show`; every mutation is
//! `ents_forge::agent::{append_thread, revise_plan, reopen}` — the same
//! `lens.parity` contract `crate::pages::agents` follows, and the same
//! commands enforce this page's own enforcement rules (`append_thread`
//! refuses a queued/running/terminal session; `revise_plan` drops a stale
//! confirm) rather than this module re-implementing them. The actual LLM
//! call goes through `crate::planner::Planner`, injected via `AppState`;
//! this module never talks to a model directly (per-member credentials are
//! Phase 6's own scope).
//!
//! SSE is a page-level concern here alone
//! (`docs/agent-sessions-plan.adoc`'s Phase 4 acceptance): [`send`]'s own
//! doc explains why the streamed reply rides back over the *same* `POST
//! /agents/{id}/chat` route (content-negotiated on `Accept`) rather than a
//! second `GET` route — this crate's only use of SSE anywhere, feeding
//! this page's own progressive-enhancement script (`crate::assets::SCRIPT`'s
//! agent-chat block).

use std::sync::Arc;

use axum::Form;
use axum::extract::{Path, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Redirect};
use ents_forge::agent::{self, AgentSession, Status as SessionStatus};
use gix_object::{Find, Write};
use maud::{Markup, html};
use serde::Deserialize;

use crate::error::Result;
use crate::session::Session;
use crate::state::AppState;

/// One rendered turn in the chat transcript, decoded from an opaque
/// `AgentSession::thread` blob by this page's own convention (see
/// [`encode_turn`]/[`decode_turn`]) — never by `ents_forge::agent` itself,
/// which keeps treating `thread` as write-only audit material everywhere
/// else in this crate (`crate::pages::agents`'s own doc: "Thread blobs are
/// never rendered anywhere ... only counted"). This page is the one place
/// that convention is deliberately different: it is the very page that
/// wrote those blobs, in a format only it needs to understand.
struct Turn {
    /// `"prompt"` for the session's own seeded first turn, `"user"` or
    /// `"assistant"` for a chat exchange this page appended, or `"note"`
    /// for any other blob (a hand-written `git ents agent finish`
    /// transcript, say) this page did not itself write.
    role: &'static str,
    /// The turn's own text, with this page's own role prefix (if any)
    /// stripped.
    text: String,
}

/// This page's own encoding for a chat turn appended to `thread`: a
/// `"<role>: "` prefix over otherwise-plain UTF-8 text.
fn encode_turn(role: &str, text: &str) -> Vec<u8> {
    format!("{role}: {text}").into_bytes()
}

/// Decode one `thread` blob at `index` into a [`Turn`] for display (see
/// [`Turn`]'s own doc for the role rules).
fn decode_turn(index: usize, blob: &[u8]) -> Turn {
    let text = String::from_utf8_lossy(blob).into_owned();
    if index == 0 {
        return Turn {
            role: "prompt",
            text,
        };
    }
    if let Some(rest) = text.strip_prefix("user: ") {
        return Turn {
            role: "user",
            text: rest.to_owned(),
        };
    }
    if let Some(rest) = text.strip_prefix("assistant: ") {
        return Turn {
            role: "assistant",
            text: rest.to_owned(),
        };
    }
    Turn { role: "note", text }
}

/// What this page renders below the transcript, derived from the
/// session's own state — mirrors `ents_forge::agent::append_thread`'s own
/// precondition exactly (this is a rendering decision, not a second
/// enforcement point; the command itself is what actually refuses an
/// illegal mutation).
enum ChatMode {
    /// `planning`, or `ready`-and-awaiting-confirmation: the composer and
    /// the plan editor both render.
    Compose,
    /// `ready`-and-queued: chatting or redrafting first requires the
    /// explicit un-queue (`POST /agents/{id}/reopen`).
    Queued,
    /// `running`, `done`, or `failed`: past the point of no return: the
    /// transcript is read-only.
    Closed,
}

fn chat_mode(session: &AgentSession) -> ChatMode {
    match session.meta.status {
        SessionStatus::Planning => ChatMode::Compose,
        SessionStatus::Ready if session.awaiting_confirmation() => ChatMode::Compose,
        SessionStatus::Ready => ChatMode::Queued,
        SessionStatus::Running | SessionStatus::Done | SessionStatus::Failed(_) => ChatMode::Closed,
    }
}

/// `GET /agents/{id}/chat`: the planning-chat page — the transcript so
/// far, then [`chat_mode`]'s own composer/queued-notice/closed-notice.
///
/// # Errors
///
/// Propagates [`ents_forge::agent::show`]'s own failures (including
/// [`ents_forge::Error::NotFound`]).
// @relation(lens.parity, scope=function)
pub async fn show<O>(
    State(state): State<Arc<AppState<O>>>,
    axum::Extension(session): axum::Extension<Session>,
    Path(id): Path<String>,
) -> Result<Markup>
where
    O: Find + Write + Send + 'static,
{
    let agent_session = agent::show(state.refs.as_ref(), &*state.objects(), &id)?;
    let title = format!("Planning chat \u{2014} {}", ents_forge::abbreviate_id(&id));
    let turns: Vec<Turn> = agent_session
        .thread
        .iter()
        .enumerate()
        .map(|(index, blob)| decode_turn(index, blob))
        .collect();

    Ok(super::layout(
        &super::RepoHeader::from_state(&state),
        &super::identity_label(&state),
        super::Tab::Agents,
        &title,
        html! {
            (super::child_crumbs("agents", &format!("/agents/{id}"), "chat"))
            div.readable {
                div.card {
                    h1.commit-subject { (title) }
                    div.chat-thread data-chat-thread {
                        @for turn in &turns {
                            div.chat-turn class={ "chat-" (turn.role) } {
                                span.chat-role { (turn.role) }
                                p { (turn.text) }
                            }
                        }
                    }
                    @match chat_mode(&agent_session) {
                        ChatMode::Compose => (composer(&session, &id, agent_session.plan.as_deref())),
                        ChatMode::Queued => (queued_notice(&session, &id)),
                        ChatMode::Closed => (closed_notice()),
                    }
                }
            }
        },
    ))
}

/// The message composer (`POST /agents/{id}/chat`, JS-enhanced by
/// `crate::assets::SCRIPT`'s agent-chat block into a streamed `fetch` of
/// that same route — see [`send`]'s own doc) and the plan editor (`POST
/// /agents/{id}/plan`), rendered together while [`ChatMode::Compose`]
/// holds.
fn composer(session: &Session, id: &str, plan: Option<&str>) -> Markup {
    html! {
        form.chat-composer method="post" action={ "/agents/" (id) "/chat" }
            data-agent-chat data-csrf=(session.csrf)
        {
            (super::csrf_input(session))
            label { "message" textarea name="message" {} }
            button type="submit" { "Send" }
        }
        form.plan-editor method="post" action={ "/agents/" (id) "/plan" } {
            (super::csrf_input(session))
            label { "plan" textarea name="plan" { (plan.unwrap_or_default()) } }
            button type="submit" { "Commit plan" }
        }
    }
}

/// The un-queue notice (`POST /agents/{id}/reopen`), rendered instead of
/// the composer while [`ChatMode::Queued`] holds — chatting or redrafting
/// a queued session first requires this explicit action
/// (`docs/agent-sessions-plan.adoc`'s resolved-by-default item 1).
fn queued_notice(session: &Session, id: &str) -> Markup {
    html! {
        p.muted {
            "This session is confirmed and queued for execution. Reopen it to resume planning \
             — this drops the existing confirmation and requires a fresh one before it can run."
        }
        form method="post" action={ "/agents/" (id) "/reopen" } {
            (super::csrf_input(session))
            button type="submit" { "Reopen for planning" }
        }
    }
}

/// The read-only notice rendered once a session is past the point of no
/// return ([`ChatMode::Closed`]).
fn closed_notice() -> Markup {
    html! {
        p.muted { "This session's planning is closed; the transcript above is read-only." }
    }
}

/// The form fields `POST /agents/{id}/chat` accepts.
#[derive(Debug, Deserialize)]
pub struct ChatForm {
    /// The member's message.
    message: String,
    /// The per-session CSRF token (`roots.web-session`).
    csrf: String,
}

/// Append `message` as a user turn and `state.planner`'s reply to it as an
/// assistant turn, in one atomic commit
/// (`ents_forge::agent::append_thread`) — the one mutation [`send`]
/// performs, before it renders either of its two response shapes.
///
/// # Errors
///
/// Propagates [`ents_forge::agent::show`]'s and
/// [`ents_forge::agent::append_thread`]'s own failures (including the
/// queued/running/terminal refusal — `docs/agent-sessions-plan.adoc`'s
/// Phase 4 acceptance: "after confirm, no endpoint accepts messages ...
/// without the explicit un-queue").
fn reply_and_append<O>(
    state: &AppState<O>,
    session: &Session,
    id: &str,
    message: &str,
) -> Result<String>
where
    O: Find + Write,
{
    let agent_session = agent::show(state.refs.as_ref(), &*state.objects(), id)?;
    let reply = state.planner.reply(&agent_session, message);
    let identity = state.identity.as_ref();
    let outcome = agent::append_thread(
        state.refs.as_ref(),
        &*state.objects(),
        state.events.as_ref(),
        id,
        vec![
            encode_turn("user", message),
            encode_turn("assistant", &reply),
        ],
        &crate::receive_identity!(identity, super::member_author(session)),
        state.mode,
    )?;
    crate::error::outcome_to_result(outcome)?;
    Ok(reply)
}

/// `POST /agents/{id}/chat`: append the turn pair synchronously
/// ([`reply_and_append`]), then respond one of two shapes from the exact
/// same, single, CSRF-checked, auth-gated `POST` — deliberately not a
/// second, unauthenticated `GET` route, since
/// `crate::router::auth_middleware`'s own sign-in-required policy assumes
/// "every mutation in this crate is a `POST`" and gates exactly that
/// method; a `GET` that also mutated would silently bypass it.
///
/// * A plain form submission (no `Accept: text/event-stream`) gets an
///   ordinary redirect back to the chat page — the no-JS fallback that
///   works with no script at all.
/// * `crate::assets::SCRIPT`'s agent-chat block instead issues this same
///   `POST` via `fetch` with that `Accept` header, and reads back an SSE
///   response — one `message` event carrying the assistant's reply
///   (`docs/agent-sessions-plan.adoc`'s Phase 4: "SSE as a page-level
///   concern"), then a `done` event closing the stream. `fetch` (unlike
///   `EventSource`) can read a streamed body from any method, so this
///   needs no second route to stream from.
///
/// # Errors
///
/// [`crate::Error::BadCsrf`] if `form.csrf` does not match; otherwise see
/// [`reply_and_append`].
// @relation(roots.web-signing, roots.web-session, lens.parity, scope=function)
pub async fn send<O>(
    State(state): State<Arc<AppState<O>>>,
    axum::Extension(session): axum::Extension<Session>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    Form(form): Form<ChatForm>,
) -> Result<axum::response::Response>
where
    O: Find + Write + Send + 'static,
{
    super::require_csrf(&session, &form.csrf)?;
    let reply = reply_and_append(&state, &session, &id, &form.message)?;

    let wants_stream = headers
        .get(axum::http::header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|accept| accept.contains("text/event-stream"));
    if wants_stream {
        let events: Vec<std::result::Result<Event, std::convert::Infallible>> = vec![
            Ok(Event::default().data(reply)),
            Ok(Event::default().event("done").data("")),
        ];
        return Ok(Sse::new(futures_util::stream::iter(events))
            .keep_alive(KeepAlive::default())
            .into_response());
    }
    Ok(Redirect::to(&format!("/agents/{id}/chat")).into_response())
}

/// The plan editor's own form fields (`POST /agents/{id}/plan`).
#[derive(Debug, Deserialize)]
pub struct PlanForm {
    /// The plan text to commit.
    plan: String,
    /// The per-session CSRF token (`roots.web-session`).
    csrf: String,
}

/// `POST /agents/{id}/plan`: commit `form.plan` as the session's plan
/// (`ents_forge::agent::revise_plan`), transitioning it to `ready` and
/// dropping any stale confirm — the same path
/// `docs/agent-sessions-plan.adoc`'s Phase 4 names for both the laptop
/// chat page and the mobile `agent-plan` effect.
///
/// # Errors
///
/// [`crate::Error::BadCsrf`] if `form.csrf` does not match; otherwise
/// propagates [`ents_forge::agent::revise_plan`]'s own failures.
// @relation(roots.web-signing, roots.web-session, lens.parity, scope=function)
pub async fn commit_plan<O>(
    State(state): State<Arc<AppState<O>>>,
    axum::Extension(session): axum::Extension<Session>,
    Path(id): Path<String>,
    Form(form): Form<PlanForm>,
) -> Result<impl IntoResponse>
where
    O: Find + Write + Send + 'static,
{
    super::require_csrf(&session, &form.csrf)?;
    let identity = state.identity.as_ref();
    let outcome = agent::revise_plan(
        state.refs.as_ref(),
        &*state.objects(),
        state.events.as_ref(),
        &id,
        form.plan,
        &crate::receive_identity!(identity, super::member_author(&session)),
        state.mode,
    )?;
    crate::error::outcome_to_result(outcome)?;
    Ok(Redirect::to(&format!("/agents/{id}")))
}

/// The reopen action's own form fields (`POST /agents/{id}/reopen`).
#[derive(Debug, Deserialize)]
pub struct ReopenForm {
    /// The per-session CSRF token (`roots.web-session`).
    csrf: String,
}

/// `POST /agents/{id}/reopen`: the explicit un-queue
/// (`ents_forge::agent::reopen`) — return a queued session to `planning`,
/// dropping its confirm, then back to the chat page to resume the
/// conversation.
///
/// # Errors
///
/// [`crate::Error::BadCsrf`] if `form.csrf` does not match; otherwise
/// propagates [`ents_forge::agent::reopen`]'s own failures (including the
/// "not ready" precondition miss).
// @relation(roots.web-signing, roots.web-session, lens.parity, scope=function)
pub async fn reopen<O>(
    State(state): State<Arc<AppState<O>>>,
    axum::Extension(session): axum::Extension<Session>,
    Path(id): Path<String>,
    Form(form): Form<ReopenForm>,
) -> Result<impl IntoResponse>
where
    O: Find + Write + Send + 'static,
{
    super::require_csrf(&session, &form.csrf)?;
    let identity = state.identity.as_ref();
    let outcome = agent::reopen(
        state.refs.as_ref(),
        &*state.objects(),
        state.events.as_ref(),
        &id,
        &crate::receive_identity!(identity, super::member_author(&session)),
        state.mode,
    )?;
    crate::error::outcome_to_result(outcome)?;
    Ok(Redirect::to(&format!("/agents/{id}/chat")))
}
