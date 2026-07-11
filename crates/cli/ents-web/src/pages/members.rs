//! `GET /members`, `GET /members/{username}`: the generic list/view pair
//! for [`ents_model::Member`] -- read-only in this phase (enrollment stays
//! a `git ents members add` operation; see this crate's own top-level doc
//! for why write flows are demonstrated on [`super::account`] rather than
//! duplicated per entity).

use std::sync::Arc;

use axum::extract::{Path, State};
use ents_model::Member;
use gix_object::{Find, Write};

use crate::error::{Error, Result};
use crate::state::AppState;

/// `GET /members`.
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
        "members",
        crate::render::list_table(&rows, "username", |id| format!("/members/{id}")),
    ))
}

/// `GET /members/{username}`.
///
/// # Errors
///
/// [`Error::NotFound`] if `username` has no member ref.
pub async fn show<O>(
    State(state): State<Arc<AppState<O>>>,
    Path(username): Path<String>,
) -> Result<maud::Markup>
where
    O: Find + Write + Send + 'static,
{
    let (_, member) = read_all(&state)?
        .into_iter()
        .find(|(name, _)| *name == username)
        .ok_or_else(|| Error::NotFound {
            what: format!("member {username}"),
        })?;
    Ok(super::layout(&username, crate::render::view(&member)))
}

fn read_all<O: Find>(state: &AppState<O>) -> Result<Vec<(String, Member)>> {
    let mut out = Vec::new();
    for entry in state.refs.iter_prefix("refs/meta/member/")? {
        let (name, tip) = entry?;
        let path = name.as_bstr().to_string();
        let Some(username) = path.strip_prefix("refs/meta/member/") else {
            continue;
        };
        let tree = super::commit_tree(&*state.objects(), tip)?;
        if let Ok(member) = facet_git_tree::deserialize::<Member>(&tree, &*state.objects()) {
            out.push((username.to_owned(), member));
        }
    }
    Ok(out)
}
