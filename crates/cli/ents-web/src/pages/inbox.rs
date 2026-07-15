//! `GET /inbox`: every `refs/meta/inbox/<member>/<id>` entry awaiting
//! adoption -- read-only in this phase (`sync.adoption-machinery`'s merge
//! itself stays a `git ents inbox adopt` operation; this crate has no
//! write path for it, since adoption needs a working-tree-aware three-way
//! merge, not a signed-commit form).

use std::sync::Arc;

use axum::extract::State;
use gix_object::{Find, Write};

use crate::error::Result;
use crate::state::AppState;

/// `GET /inbox`.
///
/// # Errors
///
/// Propagates a ref-store read failure.
pub async fn list<O>(State(state): State<Arc<AppState<O>>>) -> Result<maud::Markup>
where
    O: Find + Write + Send + 'static,
{
    let mut rows = Vec::new();
    for entry in state.refs.iter_prefix("refs/meta/inbox/")? {
        let (name, _) = entry?;
        let path = name.as_bstr().to_string();
        if let Some(rest) = path.strip_prefix("refs/meta/inbox/") {
            rows.push(rest.to_owned());
        }
    }
    let body = if rows.is_empty() {
        super::blankslate(
            "Inbox is empty",
            maud::html! { "Entries awaiting adoption appear here." },
        )
    } else {
        crate::render::string_list(&rows, |_| "/inbox".to_owned())
    };
    Ok(super::layout_meta(
        &super::RepoHeader::from_state(&state),
        &super::identity_label(&state),
        "/inbox",
        "Inbox",
        body,
    ))
}
