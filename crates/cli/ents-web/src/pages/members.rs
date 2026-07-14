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
    Ok(super::layout_meta(
        &super::RepoHeader::from_state(&state),
        &super::identity_label(&state),
        "/members",
        "members",
        crate::render::list_table(&rows, "username", |id| format!("/members/{id}")),
    ))
}

/// `GET /members/{username}`.
///
/// # Errors
///
/// [`Error::NotFound`] if `username` has no member ref at all -- a member
/// ref that exists but whose stored tree does not match this build's
/// [`Member`] shape degrades to [`crate::render::unreadable`] instead
/// (`roots.web-agnostic`'s graceful-degradation stance).
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
    let body = match member {
        Ok(member) => crate::render::view(&member),
        Err(detail) => crate::render::unreadable(&detail),
    };
    Ok(super::layout_meta(
        &super::RepoHeader::from_state(&state),
        &super::identity_label(&state),
        "/members",
        &username,
        maud::html! {
            (super::child_crumbs("members", "/members", &username))
            (body)
        },
    ))
}

/// Every `refs/meta/member/*` ref, with its tip's tree deserialized as a
/// [`Member`] -- `Err(detail)` for a ref this build's `#[derive(Facet)]`
/// shape could not read back, kept in the listing (not dropped) so
/// [`list`]/[`show`] can render it as a marker rather than silently
/// omitting it (`roots.web-agnostic`: a reader surfaces a marker, never an
/// error or a silent gap, for one entity written by a schema this build no
/// longer speaks).
fn read_all<O: Find>(
    state: &AppState<O>,
) -> Result<Vec<(String, std::result::Result<Member, String>)>> {
    let mut out = Vec::new();
    for entry in state.refs.iter_prefix("refs/meta/member/")? {
        let (name, tip) = entry?;
        let path = name.as_bstr().to_string();
        let Some(username) = path.strip_prefix("refs/meta/member/") else {
            continue;
        };
        // One `state.objects()` lock per iteration, reused for both reads:
        // `state.objects()` a second time *within the same statement*
        // would try to lock this non-reentrant `Mutex` while the first
        // guard is still alive (a `let`'s temporaries live to its own
        // `;`), self-deadlocking forever rather than erroring.
        let objects = state.objects();
        let member = super::commit_tree(&*objects, tip)
            .map_err(|error| error.to_string())
            .and_then(|tree| {
                facet_git_tree::deserialize::<Member>(&tree, &*objects)
                    .map_err(|error| error.to_string())
            });
        out.push((username.to_owned(), member));
    }
    Ok(out)
}
