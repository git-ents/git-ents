//! `GET /meta`: the landing page for the `meta` tab -- a card listing
//! every page family in `super::META_SECTIONS` with its blurb, so the
//! tab resolves to something other than an arbitrary pick of its five
//! children. The rail those children render beside their own content
//! (`super::layout_meta`) is this same table; this page is its index.

use std::sync::Arc;

use axum::extract::State;
use gix_object::{Find, Write};
use maud::html;

use crate::state::AppState;

/// `GET /meta`.
pub async fn show<O>(State(state): State<Arc<AppState<O>>>) -> maud::Markup
where
    O: Find + Write + Send + 'static,
{
    super::layout(
        &super::RepoHeader::from_state(&state),
        &super::identity_label(&state),
        super::Tab::Meta,
        "meta",
        html! {
            div.card {
                div.card-header { "meta" }
                @for section in super::META_SECTIONS {
                    div.card-row {
                        a href=(section.href) { (section.name) }
                        span { (section.blurb) }
                    }
                }
            }
        },
    )
}
