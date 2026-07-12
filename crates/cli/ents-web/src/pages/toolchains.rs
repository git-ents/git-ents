//! `GET /toolchains`, `GET /toolchains/{name}`: a custom (not generic)
//! page family, per this crate's own top-level doc -- a toolchain's
//! [`ents_kiln::Recipe`] needs domain-specific rendering (`Embedded` vs
//! `Downloaded`, each with its own provenance shape) that would otherwise
//! push a `match Recipe::Embedded { .. } => ...` into the generic
//! reflection walk [`crate::render`] exists to keep type-agnostic. Import
//! is not wired here: it stays a `git ents toolchain import` operation,
//! since it takes a local directory path, not form data a browser can
//! supply.

use std::sync::Arc;

use axum::extract::{Path, State};
use ents_kiln::toolchain;
use gix_object::{Find, Write};
use maud::html;

use crate::error::Result;
use crate::state::AppState;

/// `GET /toolchains`.
///
/// # Errors
///
/// Propagates a ref-store read failure.
pub async fn list<O>(State(state): State<Arc<AppState<O>>>) -> Result<maud::Markup>
where
    O: Find + Write + Send + 'static,
{
    let names = toolchain::list(state.refs.as_ref())?;
    Ok(super::layout_meta(
        &super::RepoHeader::from_state(&state),
        &super::identity_label(&state),
        "/toolchains",
        "toolchains",
        crate::render::string_list(&names, |name| format!("/toolchains/{name}")),
    ))
}

/// `GET /toolchains/{name}`: the toolchain's recorded recipe and import
/// log.
///
/// # Errors
///
/// Propagates an `ents-kiln` lookup failure (wrapped as
/// [`crate::Error::Effect`]) if `name` has no toolchain ref.
pub async fn show<O>(
    State(state): State<Arc<AppState<O>>>,
    Path(name): Path<String>,
) -> Result<maud::Markup>
where
    O: Find + Write + Send + 'static,
{
    let (toolchain, recipe) = toolchain::view(state.refs.as_ref(), &*state.objects(), &name)?;
    let log = toolchain::log(state.refs.as_ref(), &*state.objects(), &name)?;
    Ok(super::layout_meta(
        &super::RepoHeader::from_state(&state),
        &super::identity_label(&state),
        "/toolchains",
        &name,
        html! {
            dl {
                dt { "name" } dd { (toolchain.name) }
                dt { "recipe" } dd { (format!("{recipe:?}")) }
            }
            h2 { "import log" }
            ul {
                @for oid in &log {
                    li { (oid.to_string()) }
                }
            }
        },
    ))
}
