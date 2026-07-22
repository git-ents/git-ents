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
use ents_forge::issue::{self, EditIssue, IssueAction, NewIssue};
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
        (super::tree_head("Issues", "/issues", active.is_some()))
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
/// every other conversation in this crate. The metadata `dl.entity-view`
/// stays hand-rolled rather than [`crate::render::view`]'s generic dump:
/// every row is a domain widget (state chip, assignee avatars, label
/// chips, an "unassigned"/"none" placeholder), not a field's plain text.
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

/// `POST /issues`: open an issue at a freshly generated
/// `refs/meta/issues/<id>` (`ents_forge::issue::new`), signed
/// (`roots.web-signing`) on behalf of the current session
/// (`roots.web-session`). The posted fields are
/// [`IssueAction::New`]'s own ([`crate::form::parse_action`]), so the
/// form's shape and this handler's parse are one declaration.
///
/// # Errors
///
/// [`crate::Error::BadCsrf`] if the posted token does not match;
/// otherwise propagates [`ents_forge::issue::new`]'s own failures.
// @relation(model.issue, roots.web-signing, roots.web-session, scope=function)
pub async fn create<O>(
    State(state): State<Arc<AppState<O>>>,
    axum::Extension(session): axum::Extension<Session>,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Result<impl IntoResponse>
where
    O: Find + Write + Send + 'static,
{
    super::require_csrf(&session, crate::form::posted_csrf(&pairs))?;
    dispatch(&state, &session, crate::form::parse_action("New", &pairs)?)
}

/// `POST /issues/{id}`: mutate `id`'s state, assignees, and/or labels
/// (`ents_forge::issue::edit`) as a signed mutation on the issue's own
/// ref. The posted fields are [`IssueAction::Edit`]'s own, the path's
/// `id` standing in for the CLI's positional argument.
///
/// # Errors
///
/// [`crate::Error::BadCsrf`] if the posted token does not match;
/// otherwise propagates [`ents_forge::issue::edit`]'s own failures
/// (including [`ents_forge::Error::NotFound`] when `id` names no issue).
// @relation(model.issue, roots.web-signing, roots.web-session, scope=function)
pub async fn edit<O>(
    State(state): State<Arc<AppState<O>>>,
    axum::Extension(session): axum::Extension<Session>,
    Path(id): Path<String>,
    Form(mut pairs): Form<Vec<(String, String)>>,
) -> Result<impl IntoResponse>
where
    O: Find + Write + Send + 'static,
{
    super::require_csrf(&session, crate::form::posted_csrf(&pairs))?;
    pairs.retain(|(name, _)| name != "id");
    pairs.push(("id".to_owned(), id));
    dispatch(&state, &session, crate::form::parse_action("Edit", &pairs)?)
}

/// The issue dispatch table: each mutating [`IssueAction`] variant mapped
/// to the same `ents_forge::issue` call `git ents issue`'s own command
/// module makes, with the same edit semantics (an empty label/assignee
/// set leaves the field unchanged) -- `lens.parity`, the web as another
/// caller of the one business-logic path.
// @relation(model.issue, lens.parity, scope=function)
fn dispatch<O>(state: &AppState<O>, session: &Session, action: IssueAction) -> Result<Redirect>
where
    O: Find + Write + Send + 'static,
{
    let identity = state.identity.as_ref();
    let identity = crate::receive_identity!(identity, crate::pages::member_author(session));
    match action {
        IssueAction::New {
            title,
            body,
            state: issue_state,
            label,
            assignee,
            key: _,
        } => {
            let new = NewIssue {
                title: title.unwrap_or_default(),
                body: body.unwrap_or_default(),
                state: issue_state,
                assignees: assignee.into_iter().map(MemberId::new).collect(),
                labels: label,
            };
            let (id, outcome) = issue::new(
                state.refs.as_ref(),
                &*state.objects(),
                state.events.as_ref(),
                new,
                &identity,
                state.mode,
            )?;
            crate::error::outcome_to_result(outcome)?;
            Ok(Redirect::to(&format!("/issues/{id}")))
        }
        IssueAction::Edit {
            id,
            state: issue_state,
            label,
            assignee,
            key: _,
        } => {
            let edit = EditIssue {
                state: issue_state,
                labels: (!label.is_empty()).then_some(label),
                assignees: (!assignee.is_empty())
                    .then(|| assignee.into_iter().map(MemberId::new).collect()),
            };
            let outcome = issue::edit(
                state.refs.as_ref(),
                &*state.objects(),
                state.events.as_ref(),
                &id,
                edit,
                &identity,
                state.mode,
            )?;
            crate::error::outcome_to_result(outcome)?;
            Ok(Redirect::to(&format!("/issues/{id}")))
        }
        _ => Err(crate::Error::InvalidArgument(
            "not a form-backed issue action".to_owned(),
        )),
    }
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
    crate::form::action_form::<IssueAction>(
        "New",
        session,
        &crate::form::Spec {
            action: "/issues",
            submit: "Open Issue",
            cancel: Some("/issues"),
            values: &[],
            overrides: &[
                (
                    "title",
                    html! { label { "Title" input type="text" name="title"; } },
                ),
                (
                    "state",
                    html! { div { label { "State" } (state_picker("open")) } },
                ),
                ("label", label_picker(known_labels, &[])),
                (
                    "assignee",
                    html! {
                        label {
                            "Assignees"
                            input type="text" name="assignee" placeholder="alice, bob" list="members";
                        }
                    },
                ),
            ],
        },
    )
}

/// The edit-issue form (`POST /issues/{id}`), its fields derived from
/// [`IssueAction::Edit`]'s own shape (the positional `id` is the route's
/// path segment, so no control renders for it) and pre-filled from the
/// current issue. Its `state` field carries the same [`state_picker`] as
/// [`new_form`]'s, for the same reason (see [`new_form`]'s own doc).
fn edit_form(session: &Session, issue: &ents_forge::Issue, known_labels: &[String]) -> Markup {
    crate::form::action_form::<IssueAction>(
        "Edit",
        session,
        &crate::form::Spec {
            action: "",
            submit: "Save",
            cancel: None,
            values: &[],
            overrides: &[
                (
                    "state",
                    html! { div { label { "State" } (state_picker(&issue.state)) } },
                ),
                ("label", label_picker(known_labels, &issue.labels)),
                (
                    "assignee",
                    html! {
                        label {
                            "Assignees"
                            input type="text" name="assignee" value=(join_members(&issue.assignees)) list="members";
                        }
                    },
                ),
            ],
        },
    )
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
/// then the actual control -- a single free-text `input[name=label]`
/// (spelled as [`IssueAction`]'s own `label` field, which
/// [`crate::form::parse_action`] splits on commas/whitespace) pre-filled
/// from `current` and completed by a [`label_datalist`] of `known`'s own
/// names. Unlike [`state_picker`]'s single-choice radios, real
/// independently-toggleable checkboxes are not this control's shape here:
/// a checkbox group needs client-side script to merge checkbox state into
/// the text value a no-JS post also carries (excluded -- this crate works
/// with no JS at all). The datalist gives "pick existing" a real, working
/// affordance; typing any other word is "type-and-create".
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
                name="label"
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

