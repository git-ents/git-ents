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
    Ok(super::layout_meta(
        &super::RepoHeader::from_state(&state),
        &super::identity_label(&state),
        "/redactions",
        "redactions",
        crate::render::list_table(&rows, "id", |id| format!("/redactions/{id}")),
    ))
}

/// `GET /redactions/{id}`.
///
/// # Errors
///
/// [`Error::NotFound`] if `id` has no redaction ref at all -- a redaction
/// ref that exists but whose stored tree does not match this build's
/// [`Redaction`] shape degrades to [`crate::render::unreadable`] instead
/// (`roots.web-agnostic`'s graceful-degradation stance).
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
    let body = match redaction {
        Ok(redaction) => crate::render::view(&redaction),
        Err(detail) => crate::render::unreadable(&detail),
    };
    Ok(super::layout_meta(
        &super::RepoHeader::from_state(&state),
        &super::identity_label(&state),
        "/redactions",
        &id,
        body,
    ))
}

/// Every `refs/meta/redactions/*` ref, with its tip's tree deserialized as
/// a [`Redaction`] -- `Err(detail)` for a ref this build's
/// `#[derive(Facet)]` shape could not read back, kept in the listing
/// rather than dropped (see `crate::pages::members::read_all`'s identical
/// rationale).
fn read_all<O: Find>(
    state: &AppState<O>,
) -> Result<Vec<(String, std::result::Result<Redaction, String>)>> {
    let mut out = Vec::new();
    for entry in state.refs.iter_prefix("refs/meta/redactions/")? {
        let (name, tip) = entry?;
        let path = name.as_bstr().to_string();
        let Some(id) = path.strip_prefix("refs/meta/redactions/") else {
            continue;
        };
        let redaction = super::commit_tree(&*state.objects(), tip)
            .map_err(|error| error.to_string())
            .and_then(|tree| {
                facet_git_tree::deserialize::<Redaction>(&tree, &*state.objects())
                    .map_err(|error| error.to_string())
            });
        out.push((id.to_owned(), redaction));
    }
    Ok(out)
}
