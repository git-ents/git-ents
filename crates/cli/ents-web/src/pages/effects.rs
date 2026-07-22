//! `GET /effects`, `GET /effects/{name}`: the generic list/view pair for
//! [`ents_model::Effect`], plus a light, genuine use of `ents-query`
//! (`overview.adoc`'s crate-graph row for this crate names it as a
//! dependency): the show page re-parses the effect's own trigger text as a
//! [`ents_query::Query`] and reports whether it still parses, exactly the
//! tolerance check `git_ents::hook::read_effect` already performs on the
//! hosted root before running an effect.

use std::sync::Arc;

use axum::Form;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Redirect};
use ents_model::{Effect, namespace};
use ents_query::Query;
use gix_object::{Find, Write};
use maud::html;
use serde::Deserialize;

use crate::error::{Error, Result};
use crate::session::Session;
use crate::state::AppState;

/// `GET /effects`.
///
/// # Errors
///
/// Propagates a ref-store or object read failure.
pub async fn list<O>(
    State(state): State<Arc<AppState<O>>>,
    axum::Extension(session): axum::Extension<Session>,
) -> Result<maud::Markup>
where
    O: Find + Write + Send + 'static,
{
    let mut rows = Vec::new();
    let mut failures = Vec::new();
    for (name, effect) in read_all(&state)? {
        match effect {
            Ok(effect) => rows.push((name, effect)),
            Err(error) => failures.push((format!("refs/meta/effects/{name}"), error)),
        }
    }
    let table = if rows.is_empty() {
        super::blankslate(
            "No effects yet",
            html! { "Define one with the form below." },
        )
    } else {
        crate::render::list_table(&rows, "name", |id| format!("/effects/{id}"))
    };
    Ok(super::layout_meta(
        &super::RepoHeader::from_state(&state),
        &super::identity_label(&state),
        "/effects",
        "Effects",
        html! {
            (crate::render::unreadable_disclosure(&failures))
            (table)
            div.card {
                div.card-header { "Define an effect" }
                (add_form(&session))
            }
        },
    ))
}

/// The define-effect form (`POST /effects`) -- `git ents effect add`'s
/// own arguments as form fields.
fn add_form(session: &Session) -> maud::Markup {
    html! {
        form method="post" action="/effects" {
            (super::csrf_input(session))
            label { "Name" input type="text" name="name"; }
            label {
                "Trigger"
                input type="text" name="trigger" placeholder="query.grammar trigger";
            }
            label { "Run" input type="text" name="run" placeholder="command to run"; }
            label {
                "Toolchains"
                input type="text" name="toolchains" placeholder="rust, node";
            }
            button type="submit" { "Define Effect" }
        }
    }
}

/// The form fields `POST /effects` accepts.
#[derive(Debug, Deserialize)]
pub struct AddForm {
    /// Name to record the effect under (`refs/meta/effects/<name>`).
    name: String,
    /// The query the effect triggers on (`query.grammar`).
    trigger: String,
    /// The command the effect runs.
    run: String,
    /// Comma- or whitespace-separated toolchain names.
    #[serde(default)]
    toolchains: String,
    /// The per-session CSRF token (`roots.web-session`).
    csrf: String,
}

/// `POST /effects`: define (or replace) an effect as a signed mutation on
/// `refs/meta/effects/<name>` -- the web counterpart of
/// `git ents effect add`, sharing its pre-write rule that the trigger must
/// parse (`ents_receive::reconcile`'s tolerance rule would otherwise
/// silently skip a malformed one on every future scan).
///
/// # Errors
///
/// [`crate::Error::BadCsrf`] if `form.csrf` does not match;
/// [`Error::InvalidArgument`] on an unparsable trigger or an empty name;
/// otherwise propagates the `receive` proposal's own failures.
// @relation(roots.web-signing, roots.web-session, scope=function)
pub async fn create<O>(
    State(state): State<Arc<AppState<O>>>,
    axum::Extension(session): axum::Extension<Session>,
    Form(form): Form<AddForm>,
) -> Result<impl IntoResponse>
where
    O: Find + Write + Send + 'static,
{
    super::require_csrf(&session, &form.csrf)?;
    let _: Query = form.trigger.parse().map_err(|_source| {
        Error::InvalidArgument(format!("unparsable trigger: {}", form.trigger))
    })?;
    let name = form.name.trim();
    let ref_name = namespace::effect_ref(name)
        .map_err(|_invalid| Error::InvalidArgument(format!("invalid effect name: {name}")))?;
    let effect = Effect {
        name: name.to_owned(),
        trigger: form.trigger,
        toolchains: form
            .toolchains
            .split([',', ' '])
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .map(str::to_owned)
            .collect(),
        run: form.run,
    };
    let identity = state.identity.as_ref();
    let outcome = ents_receive::propose_entity(
        state.refs.as_ref(),
        &*state.objects(),
        state.events.as_ref(),
        ref_name,
        &effect,
        &crate::receive_identity!(identity, crate::pages::member_author(&session)),
        &format!("Define effect {name}"),
        state.mode,
    )?;
    crate::error::outcome_to_result(outcome)?;
    Ok(Redirect::to(&format!("/effects/{name}")))
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
            let (label, status_class) = match effect.trigger.parse::<Query>() {
                Ok(_) => ("parses".to_owned(), "status-pass"),
                Err(error) => (format!("does not parse: {error}"), "status-fail"),
            };
            html! {
                (crate::render::view(&effect))
                p {
                    "trigger query: "
                    span class={ "status " (status_class) } { (label) }
                }
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
