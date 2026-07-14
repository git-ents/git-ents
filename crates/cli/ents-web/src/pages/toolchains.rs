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
/// [`ents_kiln::Recipe`] shape (written by an older schema) surfaces in
/// the same [`crate::render::unreadable_disclosure`] every other entity
/// family's list page renders, while its name stays linked in the listing
/// (its show page renders the unreadable marker card) -- hand-rolled here
/// since [`toolchain::list`] itself only enumerates ref names, with no
/// reflected entity for [`crate::render`]'s generic machinery to walk
/// (this page family's own top-level doc).
///
/// # Errors
///
/// Propagates a ref-store read failure.
pub async fn list<O>(State(state): State<Arc<AppState<O>>>) -> Result<maud::Markup>
where
    O: Find + Write + Send + 'static,
{
    let names = toolchain::list(state.refs.as_ref())?;
    let mut failures = Vec::new();
    for name in &names {
        if let Err(error) = toolchain::view(state.refs.as_ref(), &*state.objects(), name) {
            failures.push((format!("refs/meta/toolchains/{name}"), error.to_string()));
        }
    }
    let listing = if names.is_empty() {
        super::blankslate(
            "No toolchains yet",
            html! { "Import one with " code { "git ents toolchain import" } "." },
        )
    } else {
        html! {
            div.card {
                ul.string-list {
                    @for name in &names {
                        li {
                            a href=(format!("/toolchains/{name}")) { (name) }
                        }
                    }
                }
            }
        }
    };
    Ok(super::layout_meta(
        &super::RepoHeader::from_state(&state),
        &super::identity_label(&state),
        "/toolchains",
        "Toolchains",
        html! {
            (crate::render::unreadable_disclosure(&failures))
            (listing)
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
    // One `state.objects()` lock, reused for both `view` and `log`: a
    // `match` scrutinee's own temporaries live for the whole match (arms
    // included), so a second `state.objects()` inside the `Ok` arm below
    // would try to lock this non-reentrant `Mutex` while the scrutinee's
    // own guard is still held, self-deadlocking forever rather than
    // erroring (see `crate::pages::members::read_all`'s identical
    // rationale).
    let objects = state.objects();
    let body = match toolchain::view(state.refs.as_ref(), &*objects, &name) {
        Ok((toolchain, recipe)) => {
            let log = toolchain::log(state.refs.as_ref(), &*objects, &name).unwrap_or_default();
            html! {
                dl {
                    dt { "name" } dd { (toolchain.name) }
                    dt { "recipe" } dd { (format!("{recipe:?}")) }
                }
                h2 { "Import Log" }
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
        html! {
            (super::child_crumbs("toolchains", "/toolchains", &name))
            (body)
        },
    ))
}
