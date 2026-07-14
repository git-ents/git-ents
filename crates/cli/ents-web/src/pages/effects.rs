//! `GET /effects`, `GET /effects/{name}`: the generic list/view pair for
//! [`ents_model::Effect`], plus a light, genuine use of `ents-query`
//! (`overview.adoc`'s crate-graph row for this crate names it as a
//! dependency): the show page re-parses the effect's own trigger text as a
//! [`ents_query::Query`] and reports whether it still parses, exactly the
//! tolerance check `git_ents::hook::read_effect` already performs on the
//! hosted root before running an effect.

use std::sync::Arc;

use axum::extract::{Path, State};
use ents_model::Effect;
use ents_query::Query;
use gix_object::{Find, Write};
use maud::html;

use crate::error::{Error, Result};
use crate::state::AppState;

/// `GET /effects`.
///
/// # Errors
///
/// Propagates a ref-store or object read failure.
pub async fn list<O>(State(state): State<Arc<AppState<O>>>) -> Result<maud::Markup>
where
    O: Find + Write + Send + 'static,
{
    let rows = read_all(&state)?;
    let body = if rows.is_empty() {
        super::blankslate(
            "No effects yet",
            html! { "Registered effects and their trigger queries appear here." },
        )
    } else {
        crate::render::list_table(&rows, "name", |id| format!("/effects/{id}"))
    };
    Ok(super::layout_meta(
        &super::RepoHeader::from_state(&state),
        &super::identity_label(&state),
        "/effects",
        "Effects",
        body,
    ))
}

/// `GET /effects/{name}`.
///
/// # Errors
///
/// [`Error::NotFound`] if `name` has no effect ref at all -- an effect ref
/// that exists but whose stored tree does not match this build's
/// [`Effect`] shape degrades to [`crate::render::unreadable`] instead
/// (`roots.web-agnostic`'s graceful-degradation stance); the trigger-query
/// parse check is skipped in that case, since there is no [`Effect`] to
/// check.
pub async fn show<O>(
    State(state): State<Arc<AppState<O>>>,
    Path(name): Path<String>,
) -> Result<maud::Markup>
where
    O: Find + Write + Send + 'static,
{
    let (_, effect) = read_all(&state)?
        .into_iter()
        .find(|(id, _)| *id == name)
        .ok_or_else(|| Error::NotFound {
            what: format!("effect {name}"),
        })?;
    let body = match effect {
        Ok(effect) => {
            let query_status = match effect.trigger.parse::<Query>() {
                Ok(_) => "parses".to_owned(),
                Err(error) => format!("does not parse: {error}"),
            };
            html! {
                (crate::render::view(&effect))
                p { "trigger query: " (query_status) }
            }
        }
        Err(detail) => crate::render::unreadable(&detail),
    };
    Ok(super::layout_meta(
        &super::RepoHeader::from_state(&state),
        &super::identity_label(&state),
        "/effects",
        &name,
        html! {
            (super::child_crumbs("effects", "/effects", &name))
            (body)
        },
    ))
}

/// Every `refs/meta/effects/*` ref, with its tip's tree deserialized as an
/// [`Effect`] -- `Err(detail)` for a ref this build's `#[derive(Facet)]`
/// shape could not read back, kept in the listing rather than dropped (see
/// `crate::pages::members::read_all`'s identical rationale).
fn read_all<O: Find>(
    state: &AppState<O>,
) -> Result<Vec<(String, std::result::Result<Effect, String>)>> {
    let mut out = Vec::new();
    for entry in state.refs.iter_prefix("refs/meta/effects/")? {
        let (name, tip) = entry?;
        let path = name.as_bstr().to_string();
        let Some(id) = path.strip_prefix("refs/meta/effects/") else {
            continue;
        };
        // One `state.objects()` lock per iteration, reused for both reads
        // -- see `crate::pages::members::read_all`'s identical comment for
        // why a second `state.objects()` within the same statement would
        // self-deadlock on this non-reentrant `Mutex`.
        let objects = state.objects();
        let effect = super::commit_tree(&*objects, tip)
            .map_err(|error| error.to_string())
            .and_then(|tree| {
                facet_git_tree::deserialize::<Effect>(&tree, &*objects)
                    .map_err(|error| error.to_string())
            });
        out.push((id.to_owned(), effect));
    }
    Ok(out)
}
