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
    Ok(super::layout(
        "effects",
        crate::render::list_table(&rows, "name", |id| format!("/effects/{id}")),
    ))
}

/// `GET /effects/{name}`.
///
/// # Errors
///
/// [`Error::NotFound`] if `name` has no effect ref.
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
    let query_status = match effect.trigger.parse::<Query>() {
        Ok(_) => "parses".to_owned(),
        Err(error) => format!("does not parse: {error}"),
    };
    Ok(super::layout(
        &name,
        html! {
            (crate::render::view(&effect))
            p { "trigger query: " (query_status) }
        },
    ))
}

fn read_all<O: Find>(state: &AppState<O>) -> Result<Vec<(String, Effect)>> {
    let mut out = Vec::new();
    for entry in state.refs.iter_prefix("refs/meta/effects/")? {
        let (name, tip) = entry?;
        let path = name.as_bstr().to_string();
        let Some(id) = path.strip_prefix("refs/meta/effects/") else {
            continue;
        };
        let tree = super::commit_tree(&*state.objects(), tip)?;
        if let Ok(effect) = facet_git_tree::deserialize::<Effect>(&tree, &*state.objects()) {
            out.push((id.to_owned(), effect));
        }
    }
    Ok(out)
}
