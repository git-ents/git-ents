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
//! grow special cases for). [`members`], [`effects`], [`toolchains`],
//! [`redactions`], and [`inbox`] additionally share one `meta` tab and
//! [`META_SECTIONS`] rail rather than each carrying its own top-level tab
//! (see [`Tab`]'s own doc); [`meta`] is that group's `GET /meta` landing
//! page.

pub mod account;
pub mod comments;
pub mod dashboard;
pub mod effects;
pub mod files;
pub mod inbox;
pub mod members;
pub mod meta;
pub mod redactions;
pub mod toolchains;

use gix::bstr::ByteSlice as _;
use gix_hash::ObjectId;
use gix_object::{CommitRef, Find, Kind};
use maud::{Markup, html};

use crate::error::{Error, Result};
use crate::session::{CSRF_FIELD, Session};
use crate::state::AppState;

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
/// trimmed to four primary tabs). `Meta` covers five page families
/// ([`super::members`], [`super::effects`], [`super::toolchains`],
/// [`super::redactions`], [`super::inbox`]) behind one tab and the
/// [`META_SECTIONS`] rail (see [`layout_meta`]) rather than a tab each --
/// nine equal tabs did not scale as page families grew. `Account` matches
/// none of [`layout`]'s tab-strip arms, so it highlights nothing there;
/// its own link lives in the header's `.id-chip` instead (see [`layout`]'s
/// own doc).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tab {
    Overview,
    Files,
    Comments,
    Meta,
    Account,
}

/// One entry in the `meta` tab's registry: a page family reachable from
/// both [`meta::show`]'s index card and the `.meta-rail` every page in
/// that family renders beside its own content (see [`layout_meta`]). This
/// table is the entire registry -- growing the `meta` group means adding
/// one entry here, never touching [`layout`], [`crate::router`]'s route
/// table beyond the new route itself, or a per-page CSS hook.
pub(crate) struct MetaSection {
    /// The section's name, shown as both the rail link text and the
    /// `/meta` index card's link text.
    pub(crate) name: &'static str,
    /// The section's own list-page URL. A `/{id}` child page (e.g.
    /// `/members/{username}`) highlights this same entry rather than
    /// failing to match anything (see [`layout_meta`]'s own doc).
    pub(crate) href: &'static str,
    /// One line describing the section, shown only on the `/meta` index
    /// card.
    pub(crate) blurb: &'static str,
}

/// The `meta` tab's registry (see [`MetaSection`]'s own doc).
pub(crate) const META_SECTIONS: &[MetaSection] = &[
    MetaSection {
        name: "members",
        href: "/members",
        blurb: "Enrolled members and their signing keys.",
    },
    MetaSection {
        name: "effects",
        href: "/effects",
        blurb: "Registered effects and their trigger queries.",
    },
    MetaSection {
        name: "toolchains",
        href: "/toolchains",
        blurb: "Recorded toolchain recipes and their import provenance.",
    },
    MetaSection {
        name: "redactions",
        href: "/redactions",
        blurb: "Recorded redactions.",
    },
    MetaSection {
        name: "inbox",
        href: "/inbox",
        blurb: "Entries awaiting adoption.",
    },
];

/// The served repository's identity for the shell's `.repo-header`
/// breadcrumb band: its directory name and, when `HEAD` resolves to a
/// branch, that branch's short name (mirrors
/// `pre-redo:crates/git-ents-server/src/web/mod.rs`'s `RepoMeta`, trimmed
/// to the two fields this single-repo crate actually has a data surface
/// for -- no owner/name split, description, or topics).
pub(crate) struct RepoHeader {
    /// The served repository's directory name, shown as the sole
    /// breadcrumb crumb (this crate serves exactly one repository).
    pub(crate) name: String,
    /// The short name of `HEAD`'s branch, or `None` when `HEAD` is
    /// detached, unborn, or the repository cannot be opened -- the
    /// `.branch` pill is omitted in that case rather than guessed at.
    pub(crate) branch: Option<String>,
}

impl RepoHeader {
    /// Read the served repository's name and current branch off `state`
    /// once, so [`layout`]'s call sites stay one-liners and the
    /// `gix::open`/`HEAD` logic lives in exactly this one place (the same
    /// `gix::open(&state.path)` pattern [`crate::pages::files`] browses the
    /// `HEAD` tree with). Never panics: an unopenable repository or a
    /// detached/unborn `HEAD` degrades to no branch pill.
    pub(crate) fn from_state<O>(state: &AppState<O>) -> Self {
        let name = std::fs::canonicalize(&state.path)
            .ok()
            .as_deref()
            .and_then(std::path::Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "repository".to_owned());
        let branch = gix::open(&state.path).ok().and_then(|repo| {
            repo.head_name()
                .ok()
                .flatten()
                .map(|full| full.shorten().to_str_lossy().into_owned())
        });
        Self { name, branch }
    }
}

/// Wrap `title` and `body` in the one page shell every route renders
/// through -- the pre-redo header bar, repo-header breadcrumb band, and tab
/// nav (`pre-redo:crates/git-ents-server/src/web/style.css`'s `.site-nav`/
/// `.nav-search`/`.repo-header`/`.tabs` rules), `active` naming which tab
/// is current and `repo` the served repository the band names. `identity`
/// is the signing identity's display label (see [`identity_label`]),
/// rendered as the header's right-aligned `.id-chip` link to `/account` --
/// account has no tab of its own (see [`Tab`]'s own doc), so this chip is
/// its only entry point from the shell.
pub(crate) fn layout(
    repo: &RepoHeader,
    identity: &str,
    active: Tab,
    title: &str,
    body: Markup,
) -> Markup {
    html! {
        (maud::DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                meta name="color-scheme" content="light dark";
                title { "git ents: " (title) }
                link rel="stylesheet" href="/style.css";
            }
            body {
                nav.site-nav {
                    div.nav-inner {
                        a.nav-logo href="/" { span.nav-mark { "✳" } "git-ents" }
                        div.nav-search {
                            (crate::assets::icon_search())
                            input type="search" placeholder="Jump to file or symbol" aria-label="Search" disabled title="Not available yet";
                        }
                        a.id-chip href="/account" { (identity) }
                    }
                }
                div.repo-header {
                    div.repo-headline {
                        div.repo-path {
                            (crate::assets::icon_folder())
                            span.here { (repo.name) }
                            @if let Some(branch) = &repo.branch {
                                span.branch { (crate::assets::icon_branch()) (branch) }
                            }
                        }
                    }
                }
                nav.tabs {
                    a.tab.active[active == Tab::Overview] href="/" { "overview" }
                    a.tab.active[active == Tab::Files] href="/files" { "files" }
                    a.tab.active[active == Tab::Comments] href="/comments" { "comments" }
                    a.tab.active[active == Tab::Meta] href="/meta" { "meta" }
                }
                main.content {
                    div.page-header { h1.page-title { (title) } }
                    (body)
                }
            }
        }
    }
}

/// Wrap `body` in the [`META_SECTIONS`] rail, then in [`layout`] itself
/// with `Meta` active -- the thin wrapper every meta-namespace page
/// ([`super::members`], [`super::effects`], [`super::toolchains`],
/// [`super::redactions`], [`super::inbox`]) calls instead of [`layout`]
/// directly, so the rail markup lives in exactly one place. `active_href`
/// names which [`META_SECTIONS`] entry to highlight -- a page family's own
/// `href`, not the request's actual path, so a `/{id}` child page (e.g.
/// `/members/{username}`) highlights the same rail entry as its list page.
pub(crate) fn layout_meta(
    repo: &RepoHeader,
    identity: &str,
    active_href: &str,
    title: &str,
    body: Markup,
) -> Markup {
    layout(
        repo,
        identity,
        Tab::Meta,
        title,
        html! {
            div.meta-layout {
                nav.meta-rail {
                    @for section in META_SECTIONS {
                        a.active[section.href == active_href] href=(section.href) { (section.name) }
                    }
                }
                div { (body) }
            }
        },
    )
}

/// The signing identity's display label for [`layout`]'s `.id-chip`
/// (`roots.web-signing`) -- [`crate::identity::SigningIdentity::actor`]'s
/// own author name, the cheapest accessor the trait exposes. Every page
/// reads this off `state` itself rather than `layout` reaching into
/// [`AppState`], so `layout` stays a pure function of the shell's own
/// chrome inputs (the same reason a [`Session`] is never threaded into it).
pub(crate) fn identity_label<O>(state: &AppState<O>) -> String {
    state.identity.actor().name.to_string()
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
