//! `GET /agents`, `GET /agents/{id}`, `POST /agents`, `POST
//! /agents/{id}/confirm`: the agent-session surface
//! (`docs/agent-sessions-plan.adoc`'s Phase 3), a top-level tab of its own
//! (`crate::pages::Tab::Agents`; see [`super`]'s own doc) rather than an
//! entry in the `meta` tab's registry -- an agent session is a working
//! surface a member drives, like issues, not repository metadata.
//!
//! Every read is `ents_forge::agent::{list_all,show}` and every mutation is
//! `ents_forge::agent::{new,confirm}` -- the web is another caller of the
//! same library funcs (`lens.parity`), never a second session
//! implementation. No `model.agent-session` spec section exists yet (the
//! plan's own owner item); this module cites only requirement ids that
//! already exist, exactly as `ents_forge::agent`'s own modules do.
//!
//! Thread blobs are never rendered anywhere in this module, only counted --
//! they are opaque, redactable audit material
//! (`ents_forge::agent::AgentSession`'s own doc), and no raw-blob serving
//! route exists in this crate to link one out to (`crate::pages::files`'s
//! raw-source view serves a working-tree path, not an arbitrary meta-ref
//! blob by oid) -- so a session's detail page names only how many turns its
//! thread carries, never their content.

use std::sync::Arc;

use axum::Form;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Redirect};
use ents_forge::agent::{
    self, AgentSession, FailureReason, NewAgentSession, ReviewPolicy, Status as SessionStatus,
};
use gix::bstr::ByteSlice as _;
use gix_hash::ObjectId;
use gix_object::{CommitRef, Find, Kind, Write};
use maud::{Markup, html};
use serde::Deserialize;

use crate::error::{Error, Result};
use crate::session::Session;
use crate::state::AppState;

/// `GET /agents`: the Agents split (`crate::pages::layout_split`) -- every
/// agent session recorded in this repository
/// (`ents_forge::agent::list_all`), newest first, as the sidebar (see
/// [`agents_sidebar`]), beside the start-a-session composer in the pane.
///
/// # Errors
///
/// Propagates a ref-store or object read failure.
// @relation(lens.parity, scope=function)
pub async fn list<O>(
    State(state): State<Arc<AppState<O>>>,
    axum::Extension(session): axum::Extension<Session>,
) -> Result<Markup>
where
    O: Find + Write + Send + 'static,
{
    let (mut rows, unreadable) = agent::list_all(state.refs.as_ref(), &*state.objects())?;
    rows.sort_by_key(|(_, session)| std::cmp::Reverse(session.meta.created));
    let failures: Vec<(String, String)> = unreadable
        .into_iter()
        .map(|entry| (entry.refname, entry.error))
        .collect();
    let default_base = default_base_ref(&state);
    Ok(super::layout_split(
        &super::RepoHeader::from_state(&state),
        &super::identity_label(&state),
        super::Tab::Agents,
        "Agents",
        agents_sidebar(&rows, None),
        html! {
            div.readable {
                (crate::render::unreadable_disclosure(&failures))
                @if rows.is_empty() {
                    (super::blankslate(
                        "No agent sessions yet",
                        html! { "Start one with the form below." },
                    ))
                }
                div.card {
                    div.card-header { "Start an Agent Session" }
                    (new_form(&session, &default_base))
                }
            }
        },
    ))
}

/// The Agents split's `.tree` sidebar: a `.tree-head` naming the family and
/// carrying the "+ New" link into the composer, then every session as a
/// two-line `.side-row` -- its abbreviated id (`.side-title`), then a
/// `.side-meta` locator of its derived-state badge (see [`state_badge`]),
/// its owning member, and how long ago it started -- linking to its own
/// page, `.active` naming the viewed session's id.
fn agents_sidebar(rows: &[(String, AgentSession)], active: Option<&str>) -> Markup {
    html! {
        div.tree-head {
            span { "Agents" }
            a.btn.btn-sm.btn-ghost[active.is_some()] href="/agents" { "+ New" }
        }
        @if rows.is_empty() {
            span.tree-note { "No agent sessions yet." }
        }
        @for (id, session) in rows {
            a.side-row.active[active == Some(id.as_str())] href={ "/agents/" (id) } {
                span.side-title { (ents_forge::abbreviate_id(id)) }
                span.side-meta {
                    (state_badge(session))
                    span.locator {
                        "@" (session.meta.member.as_str())
                        " \u{b7} "
                        (super::ago(session.meta.created))
                    }
                }
            }
        }
    }
}

/// `GET /agents/{id}`: one agent session -- its typed metadata, its plan
/// (rendered as AsciiDoc exactly like an issue's body), a chain-derived
/// status timeline (see [`build_timeline`]), the sandbox name verbatim
/// while running, its result branch and result-record ref (see
/// [`result_branch_cell`]/[`result_record_cell`]), and a one-tap Confirm
/// form when [`ents_forge::agent::AgentSession::awaiting_confirmation`]
/// holds.
///
/// # Errors
///
/// [`crate::Error::Forge`] (wrapping [`ents_forge::Error::NotFound`]) if
/// `id` has no session ref at all; an existing ref whose stored tree this
/// build cannot read back degrades to [`crate::render::unreadable`]'s
/// marker card instead of erroring. Otherwise propagates a ref-store or
/// object read failure.
// @relation(lens.parity, scope=function)
pub async fn show<O>(
    State(state): State<Arc<AppState<O>>>,
    axum::Extension(session): axum::Extension<Session>,
    Path(id): Path<String>,
) -> Result<Markup>
where
    O: Find + Write + Send + 'static,
{
    let title = format!("Agent session {}", ents_forge::abbreviate_id(&id));
    let found = agent::show(state.refs.as_ref(), &*state.objects(), &id);
    let agent_session = match found {
        Ok(agent_session) => agent_session,
        // No ref at all stays a real not-found; any other failure (a tree
        // this build's shape cannot read back) is an existing entity this
        // page degrades to the plain unreadable card for.
        Err(source @ ents_forge::Error::NotFound { .. }) => return Err(source.into()),
        Err(source) => {
            return Ok(super::layout(
                &super::RepoHeader::from_state(&state),
                &super::identity_label(&state),
                super::Tab::Agents,
                &title,
                html! {
                    (super::child_crumbs("agents", "/agents", ents_forge::abbreviate_id(&id)))
                    div.readable { (crate::render::unreadable(&source.to_string())) }
                },
            ));
        }
    };

    let ref_name = ents_model::namespace::agent_session_ref(&id)?;
    let timeline = match state.refs.get(ref_name.as_ref())? {
        Some(tip) => build_timeline(&*state.objects(), tip),
        None => Timeline::default(),
    };
    let plan_body = agent_session
        .plan
        .as_deref()
        .map(|plan| crate::asciidoc::to_html(plan).unwrap_or_else(|_| html! { p { (plan) } }));
    // Best-effort: the sidebar listing every session beside this one is
    // navigation chrome, never a reason to fail the session's own page.
    let (rows, _unreadable) =
        agent::list_all(state.refs.as_ref(), &*state.objects()).unwrap_or_default();

    Ok(super::layout_split(
        &super::RepoHeader::from_state(&state),
        &super::identity_label(&state),
        super::Tab::Agents,
        &title,
        agents_sidebar(&rows, Some(&id)),
        html! {
            (super::child_crumbs("agents", "/agents", ents_forge::abbreviate_id(&id)))
            div.readable {
                div.card {
                    h1.commit-subject { (title) }
                    dl.entity-view {
                        dt { "state" }
                        dd { (state_badge(&agent_session)) }
                        dt { "member" }
                        dd { (super::avatar(agent_session.meta.member.as_str())) " @" (agent_session.meta.member.as_str()) }
                        dt { "model" }
                        dd { (agent_session.meta.model) }
                        dt { "base ref" }
                        dd { code { (agent_session.meta.base_ref) } }
                        dt { "review policy" }
                        dd { (agent_session.meta.review_policy.to_string()) }
                        @if agent_session.meta.status == SessionStatus::Running
                            && let Some(sprite) = &agent_session.meta.sprite
                        {
                            dt { "sandbox" }
                            dd { code { (sprite) } }
                        }
                        @if let Some(branch) = &agent_session.meta.result_branch {
                            dt { "result branch" }
                            dd { (result_branch_cell(&state, branch)) }
                        }
                        @if matches!(agent_session.meta.status, SessionStatus::Done | SessionStatus::Failed(_)) {
                            dt { "result record" }
                            dd { (result_record_cell(&state, timeline.confirmed_oid)) }
                        }
                        @if let SessionStatus::Failed(FailureReason { detail }) = &agent_session.meta.status {
                            dt { "failure" }
                            dd { (detail) }
                        }
                        dt { "thread" }
                        dd { (agent_session.thread.len()) " turn(s) recorded \u{2014} never rendered" }
                    }
                    h2 { "Plan" }
                    @match &plan_body {
                        Some(body) => div.doc-body { (body) },
                        None => p.muted { "No plan has been drafted yet." },
                    }
                    @if agent_session.awaiting_confirmation() {
                        (confirm_form(&session, &id))
                    }
                }
                h2 { "Timeline" }
                (timeline_section(&timeline))
            }
        },
    ))
}

/// A session's derived display state -- the durable
/// [`ents_forge::agent::SessionMeta::status`] refined by
/// [`AgentSession::queued`]/[`AgentSession::awaiting_confirmation`] into
/// the six distinct states `docs/agent-sessions-plan.adoc`'s Phase 3
/// names: planning, awaiting confirmation, queued, running, done, failed.
fn session_state(session: &AgentSession) -> &'static str {
    match session.meta.status {
        SessionStatus::Planning => "planning",
        SessionStatus::Ready if session.queued() => "queued",
        SessionStatus::Ready => "awaiting confirmation",
        SessionStatus::Running => "running",
        SessionStatus::Done => "done",
        SessionStatus::Failed(_) => "failed",
    }
}

/// The state badge [`agents_sidebar`] and [`show`] both render: a
/// checkmark/cross `.status` chip for the terminal `done`/`failed` states
/// (matching `crate::pages::commits::checks_section`'s identical result
/// taxonomy -- a session's own finish literally records a
/// [`ents_model::Status::Pass`]/`Fail`), or a neutral `.chip.state-in-progress`
/// pill labeled with [`session_state`]'s own word for every state still in
/// flight -- the four non-terminal states share one color, since the
/// label text (not the color) is what tells them apart.
fn state_badge(session: &AgentSession) -> Markup {
    let label = session_state(session);
    match label {
        "done" => html! { span.status.status-pass { "done" } },
        "failed" => html! { span.status.status-fail { "failed" } },
        _ => html! {
            span.chip.chip-pill.state-in-progress {
                span.dot {}
                (label)
            }
        },
    }
}

/// One decoded commit of a session ref's own chain -- what [`build_timeline`]
/// walks, read whole so the timeline needs at most one object read per
/// commit rather than the three separate reads `crate::pages::commit_tree`/
/// `commit_authorship` would together cost.
struct ChainCommit {
    /// The commit's own oid.
    oid: ObjectId,
    /// This commit's sole parent, or `None` at genesis.
    parent: Option<ObjectId>,
    /// The commit author's display name.
    author: String,
    /// The commit author's time, in seconds since the Unix epoch.
    seconds: i64,
    /// The commit's tree -- the session snapshot as of this commit.
    tree: ObjectId,
}

/// Read and decode the commit at `oid`, or `None` on any read/parse
/// failure -- best-effort, since a timeline's own read failure must never
/// fail the session's page (mirrors this crate's "never a 500" degradation
/// elsewhere, e.g. [`crate::render::unreadable`]).
fn read_chain_commit(objects: &impl Find, oid: ObjectId) -> Option<ChainCommit> {
    let mut buf = Vec::new();
    let data = objects.try_find(&oid, &mut buf).ok()??;
    if data.kind != Kind::Commit {
        return None;
    }
    let commit = CommitRef::from_bytes(data.data, oid.kind()).ok()?;
    let author = commit.author().ok()?;
    let seconds = author.time().map(|time| time.seconds).unwrap_or(0);
    Some(ChainCommit {
        oid,
        parent: commit.parents().next(),
        author: author.name.to_str_lossy().into_owned(),
        seconds,
        tree: commit.tree(),
    })
}

/// Walk `tip`'s own single-parent chain back to genesis, oldest first --
/// every entity-mutation commit this codebase writes
/// (`ents_receive::propose_entity`) carries exactly one parent, so this is
/// a plain linear walk, not a full revision graph traversal. Stops early,
/// silently, on the first commit [`read_chain_commit`] cannot decode
/// (best-effort; see that function's own doc).
fn walk_session_chain(objects: &impl Find, tip: ObjectId) -> Vec<ChainCommit> {
    let mut chain = Vec::new();
    let mut cursor = Some(tip);
    while let Some(oid) = cursor {
        let Some(commit) = read_chain_commit(objects, oid) else {
            break;
        };
        cursor = commit.parent;
        chain.push(commit);
    }
    chain.reverse();
    chain
}

/// One status-change entry in a session's timeline: the derived state (see
/// [`session_state`]) as of the commit where it first took that value, and
/// that commit's own author/time.
struct TimelineEntry {
    /// The derived state this entry announces.
    label: &'static str,
    /// The commit author's display name.
    author: String,
    /// The commit author's time, in seconds since the Unix epoch.
    seconds: i64,
}

/// A session's chain-derived status timeline (see [`TimelineEntry`]),
/// plus the one oid [`result_record_cell`] needs: the commit that made the
/// session queued immediately before the first commit that claimed it --
/// the exact commit `git-ents::agent_worker::run_agent_exec` reads as its
/// own dispatched `oid`, which is what the `agent-exec` result ref's
/// short-oid segment is computed from.
#[derive(Default)]
struct Timeline {
    /// The timeline's own entries, oldest first.
    entries: Vec<TimelineEntry>,
    /// The confirmed-and-queued commit that preceded the first `running`
    /// entry, if the chain ever reached `running` at all.
    confirmed_oid: Option<ObjectId>,
}

/// Build `tip`'s [`Timeline`]: walk its chain (see [`walk_session_chain`]),
/// decode each commit's own [`AgentSession`] snapshot, and emit one
/// [`TimelineEntry`] per [`session_state`] change -- a run of commits that
/// keep the same derived state (a plan redraft that lands back on the same
/// word, for instance) collapses to the one entry that state's first
/// commit already opened. Stops early, silently, on the first commit whose
/// tree cannot be decoded as an [`AgentSession`] (best-effort; the
/// timeline built from what came before it is still shown).
// @relation(scope=function)
fn build_timeline(objects: &impl Find, tip: ObjectId) -> Timeline {
    let mut entries = Vec::new();
    let mut confirmed_oid = None;
    let mut previous_label: Option<&'static str> = None;
    let mut previous_oid: Option<ObjectId> = None;
    for commit in walk_session_chain(objects, tip) {
        let Ok(decoded) = facet_git_tree::deserialize::<AgentSession>(&commit.tree, objects) else {
            break;
        };
        let label = session_state(&decoded);
        if previous_label != Some(label) {
            if label == "running" {
                confirmed_oid = previous_oid;
            }
            entries.push(TimelineEntry {
                label,
                author: commit.author.clone(),
                seconds: commit.seconds,
            });
            previous_label = Some(label);
        }
        previous_oid = Some(commit.oid);
    }
    Timeline {
        entries,
        confirmed_oid,
    }
}

/// The "Timeline" card [`show`] renders: one row per [`TimelineEntry`], a
/// state badge, its author, and how long ago (see [`super::ago`]) -- the
/// same `.card`/`.card-row` shape
/// `crate::pages::commits::checks_section` uses for its own per-row list.
fn timeline_section(timeline: &Timeline) -> Markup {
    if timeline.entries.is_empty() {
        return super::blankslate(
            "No timeline yet",
            html! { "This session's chain could not be read." },
        );
    }
    html! {
        div.card {
            @for entry in &timeline.entries {
                div.card-row {
                    span.chip.chip-pill { (entry.label) }
                    " "
                    span.muted { (entry.author) }
                    span.entry-size { (super::ago(entry.seconds)) }
                }
            }
        }
    }
}

/// The "result branch" cell [`show`] renders: a link into
/// `/commit/{oid}` when `branch`'s `refs/heads/<branch>` ref still
/// resolves in this ref store (the ordinary `/commit` page is the closest
/// thing this crate has to "view a branch," since neither
/// `crate::pages::files` nor `crate::pages::commits` browses anything but
/// `HEAD`), or the bare branch name when it does not -- a self-run
/// deployment topology this build's ref store does not carry, or a run
/// that failed before ever pushing one.
fn result_branch_cell<O: Find>(state: &AppState<O>, branch: &str) -> Markup {
    let tip = format!("refs/heads/{branch}")
        .try_into()
        .ok()
        .and_then(|name: gix::refs::FullName| state.refs.get(name.as_ref()).ok().flatten());
    match tip {
        Some(oid) => html! { a href={ "/commit/" (oid) } { code { (branch) } } },
        None => html! { code { (branch) } },
    }
}

/// The "result record" cell [`show`] renders for a terminal session: the
/// canonical `refs/meta/results/agent-exec/<short-oid>` ref name (no
/// dedicated results page exists in this crate to link one out to, so this
/// is the ref-name fallback `docs/agent-sessions-plan.adoc`'s Phase 3
/// itself calls for), derived from [`Timeline::confirmed_oid`] exactly as
/// `git-ents::agent_worker::run_agent_exec` derives it, plus a live check
/// of whether that ref has actually landed yet.
fn result_record_cell<O: Find>(state: &AppState<O>, confirmed_oid: Option<ObjectId>) -> Markup {
    let Some(confirmed_oid) = confirmed_oid else {
        return html! { span.muted { "not derivable from this session's own chain" } };
    };
    let short = ents_effect::run::short_oid(confirmed_oid);
    let Ok(name) = ents_model::namespace::result_ref("agent-exec", &short) else {
        return html! { span.muted { "not derivable from this session's own chain" } };
    };
    let recorded = state.refs.get(name.as_ref()).ok().flatten().is_some();
    html! {
        code { (name.as_bstr().to_string()) }
        @if !recorded {
            span.muted { " (not yet recorded)" }
        }
    }
}

/// The one-tap Confirm form (`POST /agents/{id}/confirm`): rendered only
/// while [`AgentSession::awaiting_confirmation`] holds -- a plain button,
/// no fields but the CSRF token, so confirming from a phone is a single
/// tap (`docs/agent-sessions-plan.adoc`'s Phase 3, "mobile-critical").
fn confirm_form(session: &Session, id: &str) -> Markup {
    html! {
        form method="post" action=(format!("/agents/{id}/confirm")) {
            (super::csrf_input(session))
            button type="submit" { "Confirm plan" }
        }
    }
}

/// `POST /agents/{id}/confirm`: confirm `id`'s current plan
/// (`ents_forge::agent::confirm`), binding its hash and queueing it for
/// execution. The review policy is never overridden from this one-tap
/// form -- it stays whatever [`ents_forge::agent::SessionMeta::review_policy`]
/// already resolved to.
///
/// # Errors
///
/// [`crate::Error::BadCsrf`] if `form.csrf` does not match; otherwise
/// propagates [`ents_forge::agent::confirm`]'s own failures (including
/// [`ents_forge::Error::NotFound`] and the "not ready to confirm"
/// precondition miss).
// @relation(roots.web-signing, roots.web-session, lens.parity, scope=function)
pub async fn confirm<O>(
    State(state): State<Arc<AppState<O>>>,
    axum::Extension(session): axum::Extension<Session>,
    Path(id): Path<String>,
    Form(form): Form<ConfirmForm>,
) -> Result<impl IntoResponse>
where
    O: Find + Write + Send + 'static,
{
    super::require_csrf(&session, &form.csrf)?;
    let identity = state.identity.as_ref();
    let outcome = agent::confirm(
        state.refs.as_ref(),
        &*state.objects(),
        state.events.as_ref(),
        &id,
        None,
        &crate::receive_identity!(identity, crate::pages::member_author(&session)),
        state.mode,
    )?;
    crate::error::outcome_to_result(outcome)?;
    Ok(Redirect::to(&format!("/agents/{id}")))
}

/// The form fields `POST /agents/{id}/confirm` accepts.
#[derive(Debug, Deserialize)]
pub struct ConfirmForm {
    /// The per-session CSRF token (`roots.web-session`).
    csrf: String,
}

/// The start-a-session form (`POST /agents`, `docs/agent-sessions-plan.adoc`'s
/// Phase 3, "mobile-critical"): a prompt textarea, a base-branch text input
/// pre-filled with `default_base` ([`default_base_ref`]), a model text
/// input pre-filled with [`DEFAULT_MODEL`], and a closed two-option review
/// policy picker defaulting to `manual` (mirrors
/// `crate::pages::commits::start_review_form`'s identical closed-verdict
/// picker) -- deliberately no toolchain or retry field: the plan's own
/// words are "complexity lives in the session doc, not the form."
fn new_form(session: &Session, default_base: &str) -> Markup {
    html! {
        form method="post" action="/agents" {
            (super::csrf_input(session))
            label { "prompt" textarea name="prompt" {} }
            label { "base branch" input type="text" name="base_ref" value=(default_base); }
            label { "model" input type="text" name="model" value=(DEFAULT_MODEL); }
            div {
                p.muted { "review policy" }
                div.picker {
                    label.opt.active {
                        input type="radio" name="review_policy" value="manual" checked hidden;
                        span.dot {}
                        "manual"
                    }
                    label.opt {
                        input type="radio" name="review_policy" value="auto" hidden;
                        span.dot {}
                        "auto"
                    }
                }
            }
            div.composer-buttons {
                a.composer-cancel href="/agents" { "Cancel" }
                button type="submit" { "Start Session" }
            }
        }
    }
}

/// The model id [`new_form`] pre-fills and [`NewForm::model`] defaults to
/// when a submission omits the field entirely -- the same default id this
/// codebase's own fixtures and `git-ents::agent_worker` tests already use.
const DEFAULT_MODEL: &str = "claude-sonnet-5";

/// The base ref [`new_form`] pre-fills: `refs/heads/<branch>` for the
/// served repository's own current `HEAD` branch
/// ([`super::RepoHeader::from_state`]), or plain `HEAD` when that cannot
/// be resolved (a detached or unborn `HEAD`) -- the same fallback
/// [`default_base_ref_field`] uses for a submission that omits the field.
fn default_base_ref<O>(state: &AppState<O>) -> String {
    match super::RepoHeader::from_state(state).branch {
        Some(branch) => format!("refs/heads/{branch}"),
        None => default_base_ref_field(),
    }
}

/// The `base_ref` form field's fallback default (see [`NewForm::base_ref`])
/// when a submission omits it entirely -- a plain text input the browser
/// always sends filled in practice ([`new_form`] pre-fills it from
/// [`default_base_ref`]), so this only ever matters for a hand-built
/// request.
fn default_base_ref_field() -> String {
    "HEAD".to_owned()
}

/// The `review_policy` form field's fallback default (see
/// [`NewForm::review_policy`]) -- `manual`, matching [`new_form`]'s own
/// picker default and `ents_forge::agent::cli::AgentAction::New`'s
/// identical CLI default.
fn default_review_policy() -> String {
    "manual".to_owned()
}

/// The form fields `POST /agents` accepts.
#[derive(Debug, Deserialize)]
pub struct NewForm {
    /// The initial task prompt, seeded verbatim as the thread's first
    /// turn.
    prompt: String,
    /// The ref the run executes against as its starting point; defaults to
    /// the served repository's own current branch (see
    /// [`default_base_ref`]).
    #[serde(default = "default_base_ref_field")]
    base_ref: String,
    /// The model id the run executes against; defaults to
    /// [`DEFAULT_MODEL`].
    #[serde(default = "default_model_field")]
    model: String,
    /// The session's initially resolved review policy: `auto` or `manual`;
    /// defaults to `manual` (see [`default_review_policy`]).
    #[serde(default = "default_review_policy")]
    review_policy: String,
    /// The per-session CSRF token (`roots.web-session`).
    csrf: String,
}

/// [`NewForm::model`]'s serde default -- see [`DEFAULT_MODEL`].
fn default_model_field() -> String {
    DEFAULT_MODEL.to_owned()
}

/// `POST /agents`: start an agent session owned by the current signing
/// identity's resolved member (`ents_forge::agent::new`), signed
/// (`roots.web-signing`) on behalf of the current session
/// (`roots.web-session`). Carries no toolchain selection -- every session
/// this form starts depends on no pinned toolchain, same as the bare
/// `git ents agent new` invocation with no `--toolchain` flags.
///
/// # Errors
///
/// [`crate::Error::BadCsrf`] if `form.csrf` does not match;
/// [`crate::Error::InvalidArgument`] if `form.review_policy` is not `auto`
/// or `manual`; otherwise propagates [`ents_forge::agent::new`]'s own
/// failures.
// @relation(meta-ref.identity-binding, roots.web-signing, roots.web-session, scope=function)
pub async fn create<O>(
    State(state): State<Arc<AppState<O>>>,
    axum::Extension(session): axum::Extension<Session>,
    Form(form): Form<NewForm>,
) -> Result<impl IntoResponse>
where
    O: Find + Write + Send + 'static,
{
    super::require_csrf(&session, &form.csrf)?;
    let member = session_owner(&state);
    let identity = state.identity.as_ref();
    let new = NewAgentSession {
        member,
        prompt: form.prompt,
        model: form.model,
        toolchains: Vec::new(),
        base_ref: form.base_ref,
        review_policy: form
            .review_policy
            .parse::<ReviewPolicy>()
            .map_err(|_source| {
                Error::InvalidArgument(format!("unknown review policy: {}", form.review_policy))
            })?,
        retry_of: None,
    };
    let (id, outcome) = agent::new(
        state.refs.as_ref(),
        &*state.objects(),
        state.events.as_ref(),
        new,
        &crate::receive_identity!(identity, crate::pages::member_author(&session)),
        state.mode,
    )?;
    crate::error::outcome_to_result(outcome)?;
    Ok(Redirect::to(&format!("/agents/{id}")))
}

/// The acting session's member id -- the new session's own owning member
/// -- resolved the same way `crate::pages::commits::reviewer_member_id`
/// does, falling back to a short hash of the public key when no enrolled
/// member matches (duplicated rather than shared for the same reason that
/// copy's own doc and `git_ents::commands::agent::session_owner`'s both
/// give: this crate accepts one small per-module copy over a shared
/// helper for a lookup this conceptually distinct across call sites).
fn session_owner<O: Find>(state: &AppState<O>) -> ents_model::MemberId {
    let pubkey = state.identity.public_openssh();
    super::account::resolve_member_by_key(state, &pubkey)
        .map(|(id, _member)| id)
        .unwrap_or_else(|_source| ents_model::MemberId::new(short_key_fingerprint(&pubkey)))
}

/// The first twelve characters of `pubkey`'s key-material token -- mirrors
/// `crate::pages::commits::short_key_fingerprint`'s identical fallback
/// label.
fn short_key_fingerprint(pubkey: &str) -> String {
    let hex: String = pubkey
        .split_whitespace()
        .nth(1)
        .unwrap_or(pubkey)
        .chars()
        .take(12)
        .collect();
    if hex.is_empty() {
        "member".to_owned()
    } else {
        hex
    }
}
