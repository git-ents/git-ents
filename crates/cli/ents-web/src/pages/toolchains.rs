//! `GET /toolchains`, `GET /toolchains/{name}`: a custom (not generic)
//! page family, per this crate's own top-level doc -- a toolchain's
//! [`ents_kiln::Recipe`] needs domain-specific rendering (`Embedded` vs
//! `Downloaded`, each with its own provenance shape) that would otherwise
//! push a `match Recipe::Embedded { .. } => ...` into the generic
//! reflection walk [`crate::render`] exists to keep type-agnostic.
//! Directory import stays a `git ents toolchain import` operation (it
//! takes a local directory path, not form data a browser can supply);
//! what `POST /toolchains` wires instead is [`toolchain::register`],
//! taking a recipe as text ([`ents_kiln::Recipe::parse`]'s own format)
//! -- an `embedded <tree-oid>` line or a `downloaded` component list is
//! exactly form data.

use std::sync::Arc;

use axum::Form;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Redirect};
use ents_kiln::toolchain;
use gix_object::{Find, Write};
use maud::html;
use serde::Deserialize;

use crate::error::{Error, Result};
use crate::session::Session;
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
pub async fn list<O>(
    State(state): State<Arc<AppState<O>>>,
    axum::Extension(session): axum::Extension<Session>,
) -> Result<maud::Markup>
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
        crate::render::string_list(&names, |name| format!("/toolchains/{name}"))
    };
    Ok(super::layout_meta(
        &super::RepoHeader::from_state(&state),
        &super::identity_label(&state),
        "/toolchains",
        "Toolchains",
        html! {
            (crate::render::unreadable_disclosure(&failures))
            (listing)
            div.card {
                div.card-header { "Import a toolchain" }
                (import_form(&session))
            }
        },
    ))
}

/// The import-toolchain form (`POST /toolchains`): a name and a recipe in
/// [`ents_kiln::Recipe::parse`]'s own text format.
fn import_form(session: &Session) -> maud::Markup {
    html! {
        form method="post" action="/toolchains" {
            (super::csrf_input(session))
            label { "Name" input type="text" name="name"; }
            label {
                "Recipe"
                textarea name="recipe"
                    placeholder="embedded <tree-oid>\nor:\ndownloaded\n<url> <sha256> <strip> [dest]" {}
            }
            button type="submit" { "Import Toolchain" }
        }
    }
}

/// The form fields `POST /toolchains` accepts.
#[derive(Debug, Deserialize)]
pub struct ImportForm {
    /// Name to record the toolchain under (`refs/meta/toolchains/<name>`).
    name: String,
    /// The recipe text ([`ents_kiln::Recipe::parse`]).
    recipe: String,
    /// The per-session CSRF token (`roots.web-session`).
    csrf: String,
}

/// `POST /toolchains`: record a toolchain from a recipe given as text
/// ([`toolchain::register`]) as a signed mutation on
/// `refs/meta/toolchains/<name>` -- the recipe-flow counterpart of
/// `git ents toolchain import`, whose directory walk cannot arrive as
/// form data (this module's own top-level doc).
///
/// # Errors
///
/// [`crate::Error::BadCsrf`] if `form.csrf` does not match;
/// [`Error::InvalidArgument`] on a recipe that does not parse or a name
/// that cannot form a ref; otherwise propagates the `receive` proposal's
/// own failures.
// @relation(roots.web-signing, roots.web-session, scope=function)
pub async fn register<O>(
    State(state): State<Arc<AppState<O>>>,
    axum::Extension(session): axum::Extension<Session>,
    Form(form): Form<ImportForm>,
) -> Result<impl IntoResponse>
where
    O: Find + Write + Send + 'static,
{
    super::require_csrf(&session, &form.csrf)?;
    let recipe = ents_kiln::Recipe::parse(&form.recipe)
        .map_err(|source| Error::InvalidArgument(format!("invalid recipe: {source}")))?;
    let name = form.name.trim();
    let identity = state.identity.as_ref();
    let outcome = toolchain::register(
        state.refs.as_ref(),
        &*state.objects(),
        state.events.as_ref(),
        name,
        &recipe,
        &crate::receive_identity!(identity, crate::pages::member_author(&session)),
        state.mode,
    )
    .map_err(|source| match source {
        ents_effect::Error::InvalidToolchainName(bad) => {
            Error::InvalidArgument(format!("invalid toolchain name: {bad}"))
        }
        other => Error::from(other),
    })?;
    crate::error::outcome_to_result(outcome)?;
    Ok(Redirect::to(&format!("/toolchains/{name}")))
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
                div.card {
                    dl.entity-view {
                        dt { "name" } dd { (toolchain.name) }
                        dt { "recipe" } dd { (format!("{recipe:?}")) }
                    }
                }
                h2 { "Import Log" }
                @if log.is_empty() {
                    (super::blankslate(
                        "No import log",
                        html! { "This toolchain has no recorded import history." },
                    ))
                } @else {
                    div.card {
                        ul.string-list {
                            @for oid in &log {
                                li {
                                    code { (super::short_oid(oid)) }
                                    // Best-effort per entry: an unreadable
                                    // commit in the chain (practically
                                    // unreachable -- `toolchain::log` itself
                                    // already errors on one) drops just its
                                    // own authorship, not the whole log.
                                    @if let Ok((author, seconds)) = super::commit_authorship(&*objects, *oid) {
                                        span.muted { " · " (author) " · " (super::ago(seconds)) }
                                    }
                                }
                            }
                        }
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
