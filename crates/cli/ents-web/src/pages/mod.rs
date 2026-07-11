//! One module per page family -- `crate::router`'s handlers given a
//! body, mirroring `git_ents::commands`'s "one module per subcommand
//! family" convention on the web side.
//!
//! [`dashboard`], [`members`], [`account`], [`effects`], [`redactions`],
//! and [`inbox`] are the generic pages: they read a kernel entity and
//! render it through [`crate::render`]'s reflection-driven mechanism,
//! never matching on which entity type they were handed.
//! [`toolchains`] and [`comments`] are legitimate custom pages
//! (`ents-kiln`'s recipe provenance and `ents-forge`'s anchor projection
//! both need domain-specific rendering no generic reflection walk should
//! grow special cases for).

pub mod account;
pub mod comments;
pub mod dashboard;
pub mod effects;
pub mod inbox;
pub mod members;
pub mod redactions;
pub mod toolchains;

use gix_hash::ObjectId;
use gix_object::{CommitRef, Find, Kind};
use maud::{Markup, html};

use crate::error::{Error, Result};
use crate::session::{CSRF_FIELD, Session};

/// The tree of the commit at `oid` -- every page that reads back a typed
/// entity needs this; mirrors `git_ents::commands::commit_tree` and
/// `ents_forge::comment::command`'s own identical, independently
/// duplicated helper (that module's own doc names this the accepted
/// pattern in this codebase).
pub(crate) fn commit_tree(objects: &impl Find, oid: ObjectId) -> Result<ObjectId> {
    let mut buf = Vec::new();
    let data = objects
        .try_find(&oid, &mut buf)
        .map_err(|source| Error::InvalidArgument(source.to_string()))?
        .ok_or_else(|| Error::NotFound {
            what: oid.to_string(),
        })?;
    if data.kind != Kind::Commit {
        return Err(Error::NotFound {
            what: oid.to_string(),
        });
    }
    let commit = CommitRef::from_bytes(data.data, oid.kind())
        .map_err(|source| Error::InvalidArgument(source.to_string()))?;
    Ok(commit.tree())
}

/// Wrap `title` and `body` in the one page shell every route renders
/// through -- navigation to every generic and custom page family this
/// crate exposes.
pub(crate) fn layout(title: &str, body: Markup) -> Markup {
    html! {
        (maud::DOCTYPE)
        html {
            head {
                meta charset="utf-8";
                title { "git ents: " (title) }
            }
            body {
                nav {
                    a href="/" { "dashboard" } " | "
                    a href="/members" { "members" } " | "
                    a href="/account" { "account" } " | "
                    a href="/effects" { "effects" } " | "
                    a href="/redactions" { "redactions" } " | "
                    a href="/toolchains" { "toolchains" } " | "
                    a href="/comments" { "comments" } " | "
                    a href="/inbox" { "inbox" }
                }
                hr;
                h1 { (title) }
                (body)
            }
        }
    }
}

/// A hidden CSRF input every form this crate renders carries
/// (`roots.web-session`): the one place that field is spelled, so a form
/// can never omit it by a typo.
pub(crate) fn csrf_input(session: &Session) -> Markup {
    html! {
        input type="hidden" name=(CSRF_FIELD) value=(session.csrf);
    }
}

/// Verify `submitted` matches `session`'s own CSRF token
/// (`roots.web-session`): every state-changing handler calls this before
/// acting on a form body.
///
/// # Errors
///
/// [`Error::BadCsrf`] if `submitted` does not match.
// @relation(roots.web-session, scope=function)
pub(crate) fn require_csrf(session: &Session, submitted: &str) -> Result<()> {
    if submitted == session.csrf {
        Ok(())
    } else {
        Err(Error::BadCsrf)
    }
}
