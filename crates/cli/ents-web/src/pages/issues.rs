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

/// `GET /issues`: the Issues split (`crate::pages::layout_split`) --
/// every issue recorded in this repository (`ents_forge::issue::list_all`)
/// as the sidebar, its state/assignees/labels on each row's own locator
/// line, beside the new-issue composer in the pane.
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
    let (rows, unreadable) = issue::list_all(state.refs.as_ref(), &*state.objects())?;
    let failures: Vec<(String, String)> = unreadable
        .into_iter()
        .map(|entry| (entry.refname, entry.error))
        .collect();
    let labels = known_labels(&rows);
    Ok(super::layout_split(
        &super::RepoHeader::from_state(&state),
        &super::identity_label(&state),
        super::Tab::Issues,
        "Issues",
        false,
        issues_sidebar(&rows, None),
        html! {
            div.readable {
                (crate::render::unreadable_disclosure(&failures))
                @if rows.is_empty() {
                    (super::blankslate(
                        "No issues yet",
                        html! { "Open one with the form below." },
                    ))
                }
                div.card {
                    div.card-header { "Open an Issue" }
                    (new_form(&session, &labels))
                }
                (super::members_datalist(&state))
            }
        },
    ))
}

/// The Issues split's `.tree` sidebar: a `.tree-head` naming the family and
/// carrying the "+ New" link into the composer, then every issue as a
/// two-line `.side-row` -- its title (`.side-title`), then a `.side-meta`
/// locator of a state-colored `.dot`, its state, assignees, and labels --
/// linking to its own page, `.active` naming the viewed issue's id.
fn issues_sidebar(rows: &[(String, ents_forge::Issue)], active: Option<&str>) -> Markup {
    html! {
        div.tree-head {
            span { "Issues" }
            a.btn.btn-sm.btn-ghost[active.is_some()] href="/issues" { "+ New" }
        }
        @if rows.is_empty() {
            span.tree-note { "No issues yet." }
        }
        @for (id, issue) in rows {
            a.side-row.active[active == Some(id.as_str())] href={ "/issues/" (id) } {
                span.side-title { (issue.title) }
                span.side-meta {
                    (state_dot(&issue.state))
                    span.locator {
                        (issue.state)
                        " \u{b7} "
                        @if let Some(first) = issue.assignees.first() {
                            "@" (first.as_str())
                            @if issue.assignees.len() > 1 {
                                " +" (issue.assignees.len() - 1)
                            }
                        } @else {
                            "unassigned"
                        }
                        " \u{b7} "
                        @if issue.labels.is_empty() { "no labels" } @else { (issue.labels.join(", ")) }
                    }
                }
            }
        }
    }
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
/// `id` has no issue ref at all; an issue ref whose stored tree this
/// build cannot read back degrades to [`crate::render::unreadable`]'s
/// marker card instead of erroring. Otherwise propagates a ref-store or
/// object read failure.
// @relation(model.issue, model.comment-context, scope=function)
pub async fn show<O>(
    State(state): State<Arc<AppState<O>>>,
    axum::Extension(session): axum::Extension<Session>,
    Path(id): Path<String>,
) -> Result<Markup>
where
    O: Find + Write + Send + 'static,
{
    let issue = match issue::show(state.refs.as_ref(), &*state.objects(), &id) {
        Ok(issue) => issue,
        // No ref at all stays a real not-found; any other failure (a tree
        // this build's shape cannot read back) is an existing entity this
        // page degrades to the plain unreadable card for.
        Err(source @ ents_forge::Error::NotFound { .. }) => return Err(source.into()),
        Err(source) => {
            return Ok(super::layout(
                &super::RepoHeader::from_state(&state),
                &super::identity_label(&state),
                super::Tab::Issues,
                &format!("Issue {}", ents_forge::abbreviate_id(&id)),
                html! {
                    (super::child_crumbs("issues", "/issues", ents_forge::abbreviate_id(&id)))
                    div.readable { (crate::render::unreadable(&source.to_string())) }
                },
            ));
        }
    };
    let context = format!("issues/{id}");
    let thread = ents_forge::comment::thread(state.refs.as_ref(), &*state.objects(), &context)?;
    let body =
        crate::asciidoc::to_html(&issue.body).unwrap_or_else(|_| html! { p { (issue.body) } });
    let return_to = format!("/issues/{id}");
    // Best-effort: the sidebar listing every issue beside this one is
    // navigation chrome, never a reason to fail the issue's own page.
    let (rows, _unreadable) =
        issue::list_all(state.refs.as_ref(), &*state.objects()).unwrap_or_default();
    let labels = known_labels(&rows);
    Ok(super::layout_split(
        &super::RepoHeader::from_state(&state),
        &super::identity_label(&state),
        super::Tab::Issues,
        &issue.title,
        false,
        issues_sidebar(&rows, Some(&id)),
        html! {
            (super::child_crumbs("issues", "/issues", ents_forge::abbreviate_id(&id)))
            div.readable {
                div.card {
                    h1.commit-subject { (issue.title) }
                    dl.entity-view {
                        dt { "state" }
                        dd { (state_chip(&issue.state)) }
                        dt { "assignees" }
                        dd {
                            @if issue.assignees.is_empty() {
                                span { "unassigned" }
                            } @else {
                                @for assignee in &issue.assignees {
                                    (super::avatar(assignee.as_str())) " @" (assignee.as_str()) " "
                                }
                            }
                        }
                        dt { "labels" }
                        dd {
                            @if issue.labels.is_empty() {
                                span { "none" }
                            } @else {
                                @for label in &issue.labels {
                                    span.label-chip { (label) } " "
                                }
                            }
                        }
                    }
                    div.doc-body { (body) }
                }
                details.disclosure {
                    summary { "Edit state, assignees, labels" }
                    (edit_form(&session, &issue, &labels))
                    (super::members_datalist(&state))
                }
                h2 { "Discussion" }
                @if thread.is_empty() {
                    (super::blankslate(
                        "No comments yet",
                        html! { "Start the discussion below." },
                    ))
                } @else {
                    (crate::pages::comments::thread_section(&state, &session, &thread, &return_to))
                }
                div.card {
                    div.card-header { "Add a comment" }
                    (comment_form(&session, &id))
                }
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
        &crate::receive_identity!(identity, crate::pages::member_author(&session)),
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
        &crate::receive_identity!(identity, crate::pages::member_author(&session)),
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
        &crate::receive_identity!(identity, crate::pages::member_author(&session)),
        state.mode,
    )?;
    crate::error::outcome_to_result(outcome)?;
    Ok(Redirect::to(&format!("/issues/{id}")))
}

/// The open-an-issue form (`POST /issues`). State picks from
/// [`state_picker`]'s closed three-option enumeration (the redesign's
/// `StatePicker`/`StateChip`, open / in-progress / closed) rather than the
/// pre-redesign free-text-plus-datalist field: `model.issue` itself still
/// stores state as an arbitrary string (`ents_forge::Issue`'s own doc,
/// "custom states are schema, not a platform feature"), so this form
/// narrowing its own three quick-pick buttons never closes that schema --
/// a state outside the trio stays reachable through `git ents issue edit`
/// or a direct edit, same as any other schema-level custom field.
fn new_form(session: &Session, known_labels: &[String]) -> Markup {
    html! {
        form method="post" action="/issues" {
            (super::csrf_input(session))
            label { "Title" input type="text" name="title"; }
            div {
                label { "State" }
                (state_picker("open"))
            }
            label { "Assignees" input type="text" name="assignees" placeholder="alice, bob" list="members"; }
            (label_picker(known_labels, &[]))
            label { "Body" textarea name="body" {} }
            div.composer-buttons {
                a.composer-cancel href="/issues" { "Cancel" }
                button type="submit" { "Open Issue" }
            }
        }
    }
}

/// The edit-issue form (`POST /issues/{id}`), its fields pre-filled from
/// the current issue. Its `state` field carries the same
/// [`state_picker`] as [`new_form`]'s, for the same reason (see
/// [`new_form`]'s own doc).
fn edit_form(session: &Session, issue: &ents_forge::Issue, known_labels: &[String]) -> Markup {
    html! {
        form method="post" action="" {
            (super::csrf_input(session))
            div {
                label { "State" }
                (state_picker(&issue.state))
            }
            label {
                "Assignees"
                input type="text" name="assignees" value=(join_members(&issue.assignees)) list="members";
            }
            (label_picker(known_labels, &issue.labels))
            button type="submit" { "Save" }
        }
    }
}

/// The comment-on-this-issue form (`POST /issues/{id}/comment`).
fn comment_form(session: &Session, id: &str) -> Markup {
    html! {
        form method="post" action=(format!("/issues/{id}/comment")) {
            (super::csrf_input(session))
            label { "Body" textarea name="body" {} }
            button type="submit" { "Comment" }
        }
    }
}

/// The three conventional issue states the redesign's `StatePicker` closes
/// over (`model.issue`'s own field stays an open string; see [`new_form`]'s
/// doc for why the form narrows to these three anyway).
const ISSUE_STATES: [&str; 3] = ["open", "in-progress", "closed"];

/// The `.chip`/`.dot` color class for a known state, or `None` for a
/// custom one this form's [`state_picker`] does not enumerate -- shared by
/// [`state_chip`] (the detail card's big pill) and [`state_dot`] (the
/// sidebar row's small status dot), so a state's color is spelled in
/// exactly one place.
fn state_class(state: &str) -> Option<&'static str> {
    match state {
        "open" => Some("state-open"),
        "in-progress" => Some("state-in-progress"),
        "closed" => Some("state-closed"),
        _ => None,
    }
}

/// The issue detail card's state `dd`: a `.chip.chip-pill` carrying a
/// leading `.dot` and the state's own color (see [`state_class`]), plain
/// neutral for a custom state outside the three [`ISSUE_STATES`].
fn state_chip(state: &str) -> Markup {
    let class = match state_class(state) {
        Some(extra) => format!("chip chip-pill {extra}"),
        None => "chip chip-pill".to_owned(),
    };
    html! {
        span class=(class) {
            span.dot {}
            (state)
        }
    }
}

/// The sidebar row's status `.dot` (see [`issues_sidebar`]): green open,
/// amber in-progress, grey closed or custom (see [`state_class`]).
fn state_dot(state: &str) -> Markup {
    let class = match state_class(state) {
        Some(extra) => format!("dot {extra}"),
        None => "dot".to_owned(),
    };
    html! { span class=(class) {} }
}

/// One [`state_picker`] option's class: `.opt`, plus `.active` and the
/// state's own color class (see [`state_class`]) when it is `current`'s
/// own value -- an inactive option stays the picker's plain neutral look
/// (mirrors the design handoff's own `statePicker`, which colors only the
/// selected option).
fn picker_opt_class(current: &str, state: &str) -> String {
    if current == state {
        match state_class(state) {
            Some(extra) => format!("opt active {extra}"),
            None => "opt active".to_owned(),
        }
    } else {
        "opt".to_owned()
    }
}

/// The state `.picker` both [`new_form`] and [`edit_form`] render: three
/// `name="state"` radios, one per [`ISSUE_STATES`] entry, styled as
/// `.picker .opt` pills (a `hidden` native radio inside a `<label>` still
/// toggles on click -- label-click activation reaches a `hidden` control
/// same as any other -- so the pill shows no native radio glyph without
/// needing any stylesheet change). Exactly one radio is always checked, so
/// the field is never posted empty: `current` itself when it names one of
/// the three, or a fourth unlabeled hidden radio carrying `current`
/// verbatim when it does not (a custom state this form's picker does not
/// enumerate stays intact until the reader deliberately picks a different
/// one).
fn state_picker(current: &str) -> Markup {
    html! {
        div.picker {
            @for state in ISSUE_STATES {
                label class=(picker_opt_class(current, state)) {
                    input type="radio" name="state" value=(state) checked[state == current] hidden;
                    span.dot {}
                    (state)
                }
            }
            @if !ISSUE_STATES.contains(&current) {
                input type="radio" name="state" value=(current) checked hidden;
            }
        }
    }
}

/// Every label already used across every issue in this repository, deduped
/// and sorted -- [`label_picker`]'s "pick existing" set, derived from the
/// same `issue::list_all` read [`issues_sidebar`] renders from rather than
/// a second query of its own.
fn known_labels(rows: &[(String, ents_forge::Issue)]) -> Vec<String> {
    let mut labels: Vec<String> = Vec::new();
    for (_, issue) in rows {
        for label in &issue.labels {
            if !labels.contains(label) {
                labels.push(label.clone());
            }
        }
    }
    labels.sort();
    labels
}

/// The labels field both [`new_form`] and [`edit_form`] render: `known`'s
/// labels previewed as `.label-chip` (`.on` for one already in `current`),
/// then the actual control -- a single free-text `input[name=labels]`
/// pre-filled from `current` and completed by a [`label_datalist`] of
/// `known`'s own names. `model.issue`'s labels stay this one
/// comma/whitespace-separated string field end to end (`EditForm`/
/// `NewForm`'s own `labels: String`, `parse_labels`), so unlike
/// [`state_picker`]'s single-choice radios, real independently-toggleable
/// checkboxes are not this control's shape here: a checkbox group needs
/// either a repeating-key field (breaking the scalar `labels` the create/
/// edit handlers and `tests/router.rs`'s own `seed_issue` already commit
/// to) or client-side script to merge checkbox state into one text value
/// (excluded -- this crate works with no JS at all). The datalist gives
/// "pick existing" a real, working affordance; typing any other word is
/// "type-and-create".
fn label_picker(known: &[String], current: &[String]) -> Markup {
    html! {
        div {
            label { "Labels" }
            @if !known.is_empty() {
                div.picker {
                    @for label in known {
                        @let on = current.iter().any(|applied| applied == label);
                        span class={ "label-chip" (if on { " on" } else { "" }) } { (label) }
                    }
                }
            }
            input
                type="text"
                name="labels"
                value=(current.join(", "))
                placeholder="bug, gate"
                list="issue-labels";
            (label_datalist(known))
        }
    }
}

/// The `datalist` of every already-used label [`label_picker`]'s free-text
/// input completes from. Rendered once per form; the two forms never
/// share a page, so the id never collides (same reasoning the
/// pre-redesign `state_datalist` carried).
fn label_datalist(known: &[String]) -> Markup {
    html! {
        datalist id="issue-labels" {
            @for label in known { option value=(label) {} }
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
