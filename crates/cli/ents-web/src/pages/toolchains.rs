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

use crate::error::{Error, Result};
use crate::state::AppState;

/// `GET /toolchains`.
///
/// Every name resolves its own recipe (`toolchain::view`) so a name whose
/// stored tree does not match this build's [`ents_kiln::Toolchain`]/
/// [`ents_kiln::Recipe`] shape (written by an older schema) renders here as
/// a muted marker rather than a working link -- the same per-entity
/// graceful-degradation stance [`crate::render::list_table`] takes for the
/// other meta families, hand-rolled here since [`toolchain::list`] itself
/// only enumerates ref names, with no reflected entity for
/// [`crate::render`]'s generic machinery to walk (this page family's own
/// top-level doc).
///
/// # Errors
///
/// Propagates a ref-store read failure.
pub async fn list<O>(State(state): State<Arc<AppState<O>>>) -> Result<maud::Markup>
where
    O: Find + Write + Send + 'static,
{
    let names = toolchain::list(state.refs.as_ref())?;
    let rows: Vec<(String, Option<String>)> = names
        .into_iter()
        .map(|name| {
            let detail = toolchain::view(state.refs.as_ref(), &*state.objects(), &name)
                .err()
                .map(|error| error.to_string());
            (name, detail)
        })
        .collect();
    Ok(super::layout_meta(
        &super::RepoHeader::from_state(&state),
        &super::identity_label(&state),
        "/toolchains",
        "toolchains",
        html! {
            div.card {
                ul.string-list {
                    @for (name, detail) in &rows {
                        li {
                            a href=(format!("/toolchains/{name}")) { (name) }
                            @if detail.is_some() {
                                span.unreadable { "unreadable \u{2014} written by an older schema" }
                            }
                        }
                    }
                }
            }
        },
    ))
}

/// `GET /toolchains/{name}`: the toolchain's recorded recipe and import
/// log.
///
/// # Errors
///
/// [`Error::NotFound`] if `name` has no toolchain ref at all
/// ([`ents_effect::Error::UnknownToolchain`]) -- a toolchain ref that
/// exists but whose stored tree does not match this build's
/// [`ents_kiln::Toolchain`]/[`ents_kiln::Recipe`] shape degrades to
/// [`crate::render::unreadable`] instead (`roots.web-agnostic`'s
/// graceful-degradation stance). The import log is best-effort once the
/// recipe itself reads back: a log entry this build cannot decode renders
/// as an empty log rather than failing the whole page, since the recipe is
/// this page's primary content.
pub async fn show<O>(
    State(state): State<Arc<AppState<O>>>,
    Path(name): Path<String>,
) -> Result<maud::Markup>
where
    O: Find + Write + Send + 'static,
{
    let body = match toolchain::view(state.refs.as_ref(), &*state.objects(), &name) {
        Ok((toolchain, recipe)) => {
            let log =
                toolchain::log(state.refs.as_ref(), &*state.objects(), &name).unwrap_or_default();
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
            }
        }
        Err(ents_effect::Error::UnknownToolchain(_)) => {
            return Err(Error::NotFound {
                what: format!("toolchain {name}"),
            });
        }
        Err(error) => crate::render::unreadable(&error.to_string()),
    };
    Ok(super::layout_meta(
        &super::RepoHeader::from_state(&state),
        &super::identity_label(&state),
        "/toolchains",
        &name,
        body,
    ))
}
