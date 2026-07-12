//! `GET /redactions`, `GET /redactions/{id}`: the generic list/view pair
//! for [`ents_model::Redaction`] -- read-only in this phase (recording a
//! redaction stays a `git ents redact add` operation, admin-only per the
//! gate's default namespace-authorization arm).

use std::sync::Arc;

use axum::extract::{Path, State};
use ents_model::Redaction;
use gix_object::{Find, Write};

use crate::error::{Error, Result};
use crate::state::AppState;

/// `GET /redactions`.
///
/// # Errors
///
/// Propagates a ref-store or object read failure.
pub async fn list<O>(State(state): State<Arc<AppState<O>>>) -> Result<maud::Markup>
where
    O: Find + Write + Send + 'static,
{
    let rows = read_all(&state)?;
    Ok(super::layout(
        super::Tab::Redactions,
        "redactions",
        crate::render::list_table(&rows, "id", |id| format!("/redactions/{id}")),
    ))
}

/// `GET /redactions/{id}`.
///
/// # Errors
///
/// [`Error::NotFound`] if `id` has no redaction ref.
pub async fn show<O>(
    State(state): State<Arc<AppState<O>>>,
    Path(id): Path<String>,
) -> Result<maud::Markup>
where
    O: Find + Write + Send + 'static,
{
    let (_, redaction) = read_all(&state)?
        .into_iter()
        .find(|(rid, _)| *rid == id)
        .ok_or_else(|| Error::NotFound {
            what: format!("redaction {id}"),
        })?;
    Ok(super::layout(
        super::Tab::Redactions,
        &id,
        crate::render::view(&redaction),
    ))
}

fn read_all<O: Find>(state: &AppState<O>) -> Result<Vec<(String, Redaction)>> {
    let mut out = Vec::new();
    for entry in state.refs.iter_prefix("refs/meta/redactions/")? {
        let (name, tip) = entry?;
        let path = name.as_bstr().to_string();
        let Some(id) = path.strip_prefix("refs/meta/redactions/") else {
            continue;
        };
        let tree = super::commit_tree(&*state.objects(), tip)?;
        if let Ok(redaction) = facet_git_tree::deserialize::<Redaction>(&tree, &*state.objects()) {
            out.push((id.to_owned(), redaction));
        }
    }
    Ok(out)
}
