//! `GET /issues`, `GET /issues/{id}`, `POST /issues`,
//! `POST /issues/{id}`, `POST /issues/{id}/comment`: the issue surface
//! (`model.issue`), a top-level tab of its own (`crate::pages::Tab::Issues`;
//! see [`super`]'s own doc) rather than an entry in the `meta` tab's
//! registry -- issues are a working surface like comments, not repository
//! metadata.
//!
//! Every read is `ents_forge::issue::{list,show}` and every mutation is
//! `ents_forge::issue::{new,edit}` or `ents_forge::comment::add` -- the web
//! is another caller of the same library funcs (`lens.parity`), never a
//! second issue or thread implementation. An issue's discussion is its
//! thread: the comments naming `issues/<id>` as their context
//! (`model.comment-context`), aggregated by `ents_forge::comment::thread`
//! and rendered through `crate::pages::comments::thread_section`, never a
//! list the issue stores.

use std::sync::Arc;

use axum::Form;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Redirect};
use ents_forge::issue::{self, EditIssue, NewIssue};
use ents_model::MemberId;
use gix_object::{Find, Write};
use maud::{Markup, html};
use serde::Deserialize;

use crate::error::Result;
use crate::session::Session;
use crate::state::AppState;

/// `GET /issues`: every issue recorded in this repository
/// (`ents_forge::issue::list`) -- title, state, assignees, and labels --
/// plus the new-issue form.
///
/// # Errors
///
/// Propagates a ref-store or object read failure.
// @relation(model.issue, scope=function)
pub async fn list<O>(
    State(state): State<Arc<AppState<O>>>,
    axum::Extension(session): axum::Extension<Session>,
) -> Result<Markup>
where
    O: Find + Write + Send + 'static,
{
    let rows = issue::list(state.refs.as_ref(), &*state.objects())?;
    Ok(super::layout(
        &super::RepoHeader::from_state(&state),
        &super::identity_label(&state),
        super::Tab::Issues,
        "issues",
        html! {
            div.readable {
                @if rows.is_empty() {
                    p { "No issues yet." }
                } @else {
                    table.entity-list {
                        thead {
                            tr { th { "issue" } th { "state" } th { "assignees" } th { "labels" } }
                        }
                        tbody {
                            @for (id, issue) in &rows {
                                tr {
                                    td { a href=(format!("/issues/{id}")) { (issue.title) } }
                                    td { span.comment-state { (issue.state) } }
                                    td { (join_members(&issue.assignees)) }
                                    td { (issue.labels.join(", ")) }
                                }
                            }
                        }
                    }
                }
                h2 { "open an issue" }
                (new_form(&session))
            }
        },
    ))
}

/// `GET /issues/{id}`: one issue (`ents_forge::issue::show`), an edit form
/// for its state/assignees/labels, and its discussion thread -- the
/// comments naming `issues/<id>` as their context
/// (`ents_forge::comment::thread`, `model.comment-context`), rendered like
/// every other conversation in this crate.
///
/// # Errors
///
/// [`crate::Error::Forge`] (wrapping [`ents_forge::Error::NotFound`]) if
/// `id` has no issue ref; otherwise propagates a ref-store or object read
/// failure.
// @relation(model.issue, model.comment-context, scope=function)
pub async fn show<O>(
    State(state): State<Arc<AppState<O>>>,
    axum::Extension(session): axum::Extension<Session>,
    Path(id): Path<String>,
) -> Result<Markup>
where
    O: Find + Write + Send + 'static,
{
    let issue = issue::show(state.refs.as_ref(), &*state.objects(), &id)?;
    let context = format!("issues/{id}");
    let thread = ents_forge::comment::thread(state.refs.as_ref(), &*state.objects(), &context)?;
    let body =
        crate::asciidoc::to_html(&issue.body).unwrap_or_else(|_| html! { p { (issue.body) } });
    let return_to = format!("/issues/{id}");
    Ok(super::layout(
        &super::RepoHeader::from_state(&state),
        &super::identity_label(&state),
        super::Tab::Issues,
        &issue.title,
        html! {
            (super::child_crumbs("issues", "/issues", ents_forge::abbreviate_id(&id)))
            div.readable {
                div.card {
                    dl {
                        dt { "state" } dd { span.comment-state { (issue.state) } }
                        dt { "assignees" } dd { (join_members(&issue.assignees)) }
                        dt { "labels" } dd { (issue.labels.join(", ")) }
                    }
                    div.doc-body { (body) }
                }
                details {
                    summary { "edit" }
                    (edit_form(&session, &issue))
                }
                h2 { "discussion" }
                (crate::pages::comments::thread_section(&state, &session, &thread, &return_to))
                h2 { "add a comment" }
                (comment_form(&session, &id))
            }
        },
    ))
}

/// The form fields `POST /issues` accepts.
#[derive(Debug, Deserialize)]
pub struct NewForm {
    /// The issue's title.
    title: String,
    /// The issue's body.
    #[serde(default)]
    body: String,
    /// The issue's initial state; defaults to `open` (`model.issue`: the
    /// platform has no default of its own, so the frontend chooses one).
    #[serde(default = "default_state")]
    state: String,
    /// Comma- or whitespace-separated assignee usernames.
    #[serde(default)]
    assignees: String,
    /// Comma- or whitespace-separated labels.
    #[serde(default)]
    labels: String,
    /// The per-session CSRF token (`roots.web-session`).
    csrf: String,
}

fn default_state() -> String {
    "open".to_owned()
}

/// `POST /issues`: open an issue at a freshly generated
/// `refs/meta/issues/<id>` (`ents_forge::issue::new`), signed
/// (`roots.web-signing`) on behalf of the current session
/// (`roots.web-session`).
///
/// # Errors
///
/// [`crate::Error::BadCsrf`] if `form.csrf` does not match; otherwise
/// propagates [`ents_forge::issue::new`]'s own failures.
// @relation(model.issue, roots.web-signing, roots.web-session, scope=function)
pub async fn create<O>(
    State(state): State<Arc<AppState<O>>>,
    axum::Extension(session): axum::Extension<Session>,
    Form(form): Form<NewForm>,
) -> Result<impl IntoResponse>
where
    O: Find + Write + Send + 'static,
{
    super::require_csrf(&session, &form.csrf)?;
    let identity = state.identity.as_ref();
    let new = NewIssue {
        title: form.title,
        body: form.body,
        state: form.state,
        assignees: parse_members(&form.assignees),
        labels: parse_labels(&form.labels),
    };
    let (id, outcome) = issue::new(
        state.refs.as_ref(),
        &*state.objects(),
        state.events.as_ref(),
        new,
        &crate::receive_identity!(identity),
        state.mode,
    )?;
    crate::error::outcome_to_result(outcome)?;
    Ok(Redirect::to(&format!("/issues/{id}")))
}

/// The form fields `POST /issues/{id}` accepts. Each field replaces its
/// counterpart on the issue; an empty `assignees`/`labels` leaves that set
/// unchanged (matching `git ents issue edit`'s own semantics), while `state`
/// is always applied.
#[derive(Debug, Deserialize)]
pub struct EditForm {
    /// Replace the issue's state.
    state: String,
    /// Comma- or whitespace-separated assignees; empty leaves them.
    #[serde(default)]
    assignees: String,
    /// Comma- or whitespace-separated labels; empty leaves them.
    #[serde(default)]
    labels: String,
    /// The per-session CSRF token (`roots.web-session`).
    csrf: String,
}

/// `POST /issues/{id}`: mutate `id`'s state, assignees, and/or labels
/// (`ents_forge::issue::edit`) as a signed mutation on the issue's own ref.
///
/// # Errors
///
/// [`crate::Error::BadCsrf`] if `form.csrf` does not match; otherwise
/// propagates [`ents_forge::issue::edit`]'s own failures (including
/// [`ents_forge::Error::NotFound`] when `id` names no issue).
// @relation(model.issue, roots.web-signing, roots.web-session, scope=function)
pub async fn edit<O>(
    State(state): State<Arc<AppState<O>>>,
    axum::Extension(session): axum::Extension<Session>,
    Path(id): Path<String>,
    Form(form): Form<EditForm>,
) -> Result<impl IntoResponse>
where
    O: Find + Write + Send + 'static,
{
    super::require_csrf(&session, &form.csrf)?;
    let identity = state.identity.as_ref();
    let assignees = parse_members(&form.assignees);
    let labels = parse_labels(&form.labels);
    let edit = EditIssue {
        state: Some(form.state),
        assignees: (!assignees.is_empty()).then_some(assignees),
        labels: (!labels.is_empty()).then_some(labels),
    };
    let outcome = issue::edit(
        state.refs.as_ref(),
        &*state.objects(),
        state.events.as_ref(),
        &id,
        edit,
        &crate::receive_identity!(identity),
        state.mode,
    )?;
    crate::error::outcome_to_result(outcome)?;
    Ok(Redirect::to(&format!("/issues/{id}")))
}

/// The form fields `POST /issues/{id}/comment` accepts.
#[derive(Debug, Deserialize)]
pub struct CommentForm {
    /// The comment's body text.
    body: String,
    /// The per-session CSRF token (`roots.web-session`).
    csrf: String,
}

/// `POST /issues/{id}/comment`: a comment naming `issues/<id>` as its
/// context (`model.comment-context`) -- an ordinary
/// [`ents_forge::comment::add`], contextual and unanchored, so it joins the
/// issue's thread the moment it lands.
///
/// # Errors
///
/// [`crate::Error::BadCsrf`] if `form.csrf` does not match; otherwise
/// propagates [`ents_forge::comment::add`]'s own failures.
// @relation(model.comment-context, roots.web-signing, roots.web-session, scope=function)
pub async fn comment<O>(
    State(state): State<Arc<AppState<O>>>,
    axum::Extension(session): axum::Extension<Session>,
    Path(id): Path<String>,
    Form(form): Form<CommentForm>,
) -> Result<impl IntoResponse>
where
    O: Find + Write + Send + 'static,
{
    super::require_csrf(&session, &form.csrf)?;
    let identity = state.identity.as_ref();
    let new = ents_forge::comment::NewComment {
        body: form.body,
        path: None,
        lines: None,
        rev: "HEAD".to_owned(),
        worktree: false,
        context: Some(format!("issues/{id}")),
        parent: None,
    };
    let (_comment_id, outcome) = ents_forge::comment::add(
        state.refs.as_ref(),
        &*state.objects(),
        state.events.as_ref(),
        &state.path,
        new,
        &crate::receive_identity!(identity),
        state.mode,
    )?;
    crate::error::outcome_to_result(outcome)?;
    Ok(Redirect::to(&format!("/issues/{id}")))
}

/// The open-an-issue form (`POST /issues`). The `state` field is a free
/// text input with a [`state_datalist`] of the conventional values, never
/// a closed `select` -- `model.issue` keeps states an open vocabulary
/// ("custom states are schema, not platform features"; see
/// [`ents_forge::Issue`]'s own doc).
fn new_form(session: &Session) -> Markup {
    html! {
        form method="post" action="/issues" {
            (super::csrf_input(session))
            label { "title" input type="text" name="title"; }
            label {
                "state"
                input type="text" name="state" value="open" list="issue-states";
            }
            (state_datalist())
            label { "assignees" input type="text" name="assignees" placeholder="alice, bob"; }
            label { "labels" input type="text" name="labels" placeholder="bug, gate"; }
            label { "body" textarea name="body" {} }
            button type="submit" { "open issue" }
        }
    }
}

/// The edit-issue form (`POST /issues/{id}`), its fields pre-filled from
/// the current issue. Its `state` field carries the same [`state_datalist`]
/// as [`new_form`]'s, for the same open-vocabulary reason.
fn edit_form(session: &Session, issue: &ents_forge::Issue) -> Markup {
    html! {
        form method="post" action="" {
            (super::csrf_input(session))
            label {
                "state"
                input type="text" name="state" value=(issue.state) list="issue-states";
            }
            (state_datalist())
            label {
                "assignees"
                input type="text" name="assignees" value=(join_members(&issue.assignees));
            }
            label { "labels" input type="text" name="labels" value=(issue.labels.join(", ")); }
            button type="submit" { "save" }
        }
    }
}

/// The `datalist` of conventional issue states both forms above attach to
/// their `state` input -- suggestions only, since `model.issue`'s state is
/// an open string vocabulary, not an enum a `select` could close over.
/// Rendered once per form; the two forms never share a page, so the id
/// never collides.
fn state_datalist() -> Markup {
    html! {
        datalist id="issue-states" {
            option value="open" {}
            option value="closed" {}
        }
    }
}

/// The comment-on-this-issue form (`POST /issues/{id}/comment`).
fn comment_form(session: &Session, id: &str) -> Markup {
    html! {
        form method="post" action=(format!("/issues/{id}/comment")) {
            (super::csrf_input(session))
            label { "body" textarea name="body" {} }
            button type="submit" { "comment" }
        }
    }
}

/// Render a member set for display, comma-joined (`join_members(&[])` is the
/// empty string, so an unassigned issue shows a blank cell rather than a
/// stray separator).
fn join_members(members: &[MemberId]) -> String {
    members
        .iter()
        .map(MemberId::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Parse a comma- or whitespace-separated list into members, dropping empty
/// segments so a trailing comma or extra spacing does not enroll a blank
/// assignee.
fn parse_members(text: &str) -> Vec<MemberId> {
    parse_labels(text).into_iter().map(MemberId::new).collect()
}

/// Parse a comma- or whitespace-separated list into labels, dropping empty
/// segments.
fn parse_labels(text: &str) -> Vec<String> {
    text.split([',', ' ', '\t', '\n'])
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .map(str::to_owned)
        .collect()
}
