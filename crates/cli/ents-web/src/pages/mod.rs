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

/// The tab-nav page families this crate exposes -- one variant per tab in
/// [`layout`]'s nav bar, so a handler can name which tab it renders behind
/// without `layout` re-deriving it from the request path (mirrors
/// `pre-redo:crates/git-ents-server/src/web/pages.rs`'s own `Tab` enum,
/// trimmed to this crate's page families).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tab {
    Dashboard,
    Members,
    Account,
    Effects,
    Redactions,
    Toolchains,
    Comments,
    Inbox,
}

/// Wrap `title` and `body` in the one page shell every route renders
/// through -- the pre-redo header bar and tab nav
/// (`pre-redo:crates/git-ents-server/src/web/style.css`'s `.site-nav`/
/// `.tabs` rules), `active` naming which tab is current.
pub(crate) fn layout(active: Tab, title: &str, body: Markup) -> Markup {
    html! {
        (maud::DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                meta name="color-scheme" content="light dark";
                title { "git ents: " (title) }
                link rel="preconnect" href="https://fonts.googleapis.com";
                link rel="preconnect" href="https://fonts.gstatic.com" crossorigin;
                link rel="stylesheet" href=(crate::assets::FONTS_HREF);
                link rel="stylesheet" href="/style.css";
            }
            body {
                nav.site-nav {
                    div.nav-inner {
                        a.nav-logo href="/" { span.nav-mark { "✳" } "git-ents" }
                    }
                }
                nav.tabs {
                    a.tab.active[active == Tab::Dashboard] href="/" { "dashboard" }
                    a.tab.active[active == Tab::Members] href="/members" { "members" }
                    a.tab.active[active == Tab::Account] href="/account" { "account" }
                    a.tab.active[active == Tab::Effects] href="/effects" { "effects" }
                    a.tab.active[active == Tab::Redactions] href="/redactions" { "redactions" }
                    a.tab.active[active == Tab::Toolchains] href="/toolchains" { "toolchains" }
                    a.tab.active[active == Tab::Comments] href="/comments" { "comments" }
                    a.tab.active[active == Tab::Inbox] href="/inbox" { "inbox" }
                }
                main.content {
                    div.page-header { h1.page-title { (title) } }
                    (body)
                }
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
