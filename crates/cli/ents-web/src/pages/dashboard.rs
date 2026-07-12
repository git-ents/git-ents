//! `GET /`: the dashboard -- entry points into every page family this
//! crate exposes, with a live count read from each namespace so the page
//! doubles as a smoke test that every seam in [`crate::state::AppState`]
//! actually reads.

use std::sync::Arc;

use axum::extract::State;
use gix_object::{Find, Write};
use maud::html;

use crate::error::Result;
use crate::state::AppState;

/// `GET /`.
///
/// # Errors
///
/// Propagates a ref-store read failure.
pub async fn show<O>(State(state): State<Arc<AppState<O>>>) -> Result<maud::Markup>
where
    O: Find + Write + Send + 'static,
{
    let members = state.refs.iter_prefix("refs/meta/member/")?.count();
    let effects = state.refs.iter_prefix("refs/meta/effects/")?.count();
    let redactions = state.refs.iter_prefix("refs/meta/redactions/")?.count();
    let comments = state.refs.iter_prefix("refs/meta/comments/")?.count();
    let toolchains = state.refs.iter_prefix("refs/meta/toolchains/")?.count();

    Ok(super::layout(
        &super::RepoHeader::from_state(&state),
        super::Tab::Dashboard,
        "dashboard",
        html! {
            div.card {
                ul.string-list {
                    li { a href="/members" { "members" } span.badge { (members) } }
                    li { a href="/account" { "account" } }
                    li { a href="/effects" { "effects" } span.badge { (effects) } }
                    li { a href="/redactions" { "redactions" } span.badge { (redactions) } }
                    li { a href="/toolchains" { "toolchains" } span.badge { (toolchains) } }
                    li { a href="/comments" { "comments" } span.badge { (comments) } }
                    li { a href="/inbox" { "inbox" } }
                }
            }
        },
    ))
}
