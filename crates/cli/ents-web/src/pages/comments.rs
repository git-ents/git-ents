//! `GET /comments`, `GET /comments/{id}`, `POST /comments`: a custom (not
//! generic) page family, per this crate's own top-level doc -- a
//! comment's anchor needs projection against a live working tree
//! (`anchor.projection`) to render meaningfully, which is exactly the
//! kind of domain-specific view `ents-forge`'s own `comment::show`
//! already returns structured data for, rather than a bare reflected
//! field list.

use std::sync::Arc;

use axum::Form;
use axum::extract::{Path, Query as PathQuery, State};
use axum::response::{IntoResponse, Redirect};
use ents_forge::comment;
use gix_object::{Find, Write};
use maud::html;
use serde::Deserialize;

use crate::error::Result;
use crate::session::Session;
use crate::state::AppState;

/// `GET /comments`.
///
/// # Errors
///
/// Propagates a ref-store or object read failure.
pub async fn list<O>(
    State(state): State<Arc<AppState<O>>>,
    axum::Extension(session): axum::Extension<Session>,
) -> Result<maud::Markup>
where
    O: Find + Write + Send + 'static,
{
    let rows = comment::list(state.refs.as_ref(), &*state.objects())?;
    Ok(super::layout(
        &super::RepoHeader::from_state(&state),
        &super::identity_label(&state),
        super::Tab::Comments,
        "comments",
        html! {
            ul {
                @for (id, comment) in &rows {
                    li { a href=(format!("/comments/{id}")) { (id) } ": " (comment.body) }
                }
            }
            h2 { "add a comment" }
            (add_form("HEAD", &session))
        },
    ))
}

/// The query parameters `GET /comments/{id}` accepts: which revision to
/// project the anchor onto (defaults to `HEAD`).
#[derive(Debug, Deserialize)]
pub struct ShowQuery {
    /// The revision to project onto; defaults to `HEAD`.
    #[serde(default = "default_rev_field")]
    rev: String,
}

fn default_rev_field() -> String {
    "HEAD".to_owned()
}

/// `GET /comments/{id}?rev=...`: the comment's body, its anchor, and the
/// projection of that anchor onto `rev` (`anchor.projection`).
///
/// # Errors
///
/// [`crate::Error::Forge`] (wrapping [`ents_forge::Error::NotFound`]) if
/// `id` has no comment ref.
pub async fn show<O>(
    State(state): State<Arc<AppState<O>>>,
    Path(id): Path<String>,
    PathQuery(query): PathQuery<ShowQuery>,
) -> Result<maud::Markup>
where
    O: Find + Write + Send + 'static,
{
    let (comment, anchor, projection) = comment::show(
        state.refs.as_ref(),
        &*state.objects(),
        &state.path,
        &id,
        &query.rev,
    )?;
    Ok(super::layout(
        &super::RepoHeader::from_state(&state),
        &super::identity_label(&state),
        super::Tab::Comments,
        &id,
        html! {
            dl {
                dt { "path" } dd { (anchor.path) }
                dt { "lines" } dd { (format!("{:?}", anchor.lines)) }
                dt { "projection at " (query.rev) } dd { (format!("{projection:?}")) }
                dt { "body" } dd { (comment.body) }
            }
        },
    ))
}

/// The form fields `POST /comments` accepts.
#[derive(Debug, Deserialize)]
pub struct AddForm {
    /// The repository-relative path to anchor to.
    path: String,
    /// The comment's text.
    body: String,
    /// An optional `<start>[:<end>]` line range.
    #[serde(default)]
    lines: String,
    /// The revision to anchor against.
    rev: String,
    /// The per-session CSRF token (`roots.web-session`).
    csrf: String,
}

/// `POST /comments`: anchor `body` to `path` at `rev`, signed
/// (`roots.web-signing`) on behalf of the current session
/// (`roots.web-session`).
///
/// # Errors
///
/// [`crate::Error::BadCsrf`] if `form.csrf` does not match; otherwise
/// propagates [`ents_forge::comment::add`]'s own failures.
// @relation(roots.web-signing, roots.web-session, scope=function)
pub async fn add<O>(
    State(state): State<Arc<AppState<O>>>,
    axum::Extension(session): axum::Extension<Session>,
    Form(form): Form<AddForm>,
) -> Result<impl IntoResponse>
where
    O: Find + Write + Send + 'static,
{
    super::require_csrf(&session, &form.csrf)?;
    let lines = (!form.lines.trim().is_empty()).then(|| form.lines.trim().to_owned());

    let identity = state.identity.as_ref();
    let (id, outcome) = comment::add(
        state.refs.as_ref(),
        &*state.objects(),
        state.events.as_ref(),
        &state.path,
        &form.path,
        form.body,
        lines,
        &form.rev,
        &crate::receive_identity!(identity),
        state.mode,
    )?;
    crate::error::outcome_to_result(outcome)?;
    Ok(Redirect::to(&format!("/comments/{id}")))
}

fn add_form(default_rev: &str, session: &Session) -> maud::Markup {
    html! {
        form method="post" action="/comments" {
            (super::csrf_input(session))
            label { "path" input type="text" name="path"; }
            label { "rev" input type="text" name="rev" value=(default_rev); }
            label { "lines" input type="text" name="lines"; }
            label { "body" textarea name="body" {} }
            button type="submit" { "comment" }
        }
    }
}
