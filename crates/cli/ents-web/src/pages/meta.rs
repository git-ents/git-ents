//! `GET /meta`: the landing page for the `meta` tab -- a card listing
//! every page family in `super::META_SECTIONS` with its blurb, so the
//! tab resolves to something other than an arbitrary pick of its five
//! children. Rendered through [`super::layout_meta`], the same governance
//! sub-rail every one of those five families renders beside its own
//! content -- this page is that rail's own index, not a sixth thing beside
//! it.

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
    super::layout_meta(
        &super::RepoHeader::from_state(&state),
        &super::identity_label(&state),
        "/meta",
        "Meta",
        html! {
            p.muted {
                "All project metadata -- members, effects, toolchains, "
                "redactions, and the adoption inbox -- lives in this "
                "repository as git objects."
            }
            div.card {
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
