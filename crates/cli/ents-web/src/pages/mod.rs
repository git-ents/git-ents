//! One module per page family -- `crate::router`'s handlers given a
//! body, mirroring `git_ents::commands`'s "one module per subcommand
//! family" convention on the web side.
//!
//! [`account`], [`effects`], [`redactions`],
//! and [`inbox`] are the generic pages: they read a kernel entity and
//! render it through [`crate::render`]'s reflection-driven mechanism,
//! never matching on which entity type they were handed. [`dashboard`],
//! [`toolchains`], [`comments`], [`issues`], and [`members`] are
//! legitimate custom pages
//! (`ents-kiln`'s recipe provenance, `ents-forge`'s anchor projection
//! and issue threads, and a member's SSH-key identity card all need
//! domain-specific rendering no generic
//! reflection walk should grow special cases for). [`members`],
//! [`effects`], [`toolchains`], [`redactions`], and [`inbox`] additionally
//! share one `meta` rail item and `META_SECTIONS` rail rather than each
//! carrying its own top-level entry (see `Tab`'s own doc); [`meta`] is that
//! group's `GET /meta` landing page. [`commits`], [`reviews`], [`issues`],
//! and [`agents`] are rail items of their own -- `Tab::Commits`,
//! `Tab::Reviews`, `Tab::Issues`, and `Tab::Agents` (Agents,
//! `docs/agent-sessions-plan.adoc`'s Phase 3) in [`layout`]'s icon rail,
//! alongside the dashboard, code, threads, and meta items. `reviews::list`
//! (`GET /reviews`) is a read-only aggregate across every commit's own
//! reviews (`commits::reviews_section` renders the same
//! [`ents_forge::review`] entities scoped to one commit; this module has no
//! writes of its own -- every mutation still posts through `commits`'s own
//! routes). [`search`]
//! renders with no rail item active at all; it is reached from the
//! `.wb-bar`'s own `.palette` search form rather than any rail item.

pub mod account;
pub mod agent_chat;
pub mod agents;
pub mod comments;
pub mod commits;
pub mod dashboard;
pub mod effects;
pub mod files;
pub mod inbox;
pub mod issues;
pub mod login;
pub mod members;
pub mod meta;
pub mod redactions;
pub mod reviews;
pub mod search;
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

/// The commit author's display name and commit time (epoch seconds) for
/// the commit at `oid` -- the meta-ref counterpart to
/// `crate::pages::commits`'s identical read of an ordinary history
/// commit, shared by any page that needs to know who mutated a meta-ref
/// entity and when rather than a stored field (`model.comment`'s own rule
/// that authorship lives in the commit chain, not the entity: see
/// `ents_forge::comment::Comment`'s own doc).
///
/// A second, independent fetch-and-parse from [`commit_tree`]'s own
/// (same file, same pattern) rather than a shared parse step: `CommitRef`
/// borrows from a caller-owned buffer, so factoring the parse out would
/// need either an owned copy or a callback -- this module's own doc on
/// [`commit_tree`] already names three such near-identical copies as the
/// accepted pattern here.
pub(crate) fn commit_authorship(objects: &impl Find, oid: ObjectId) -> Result<(String, i64)> {
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
    let author = commit
        .author()
        .map_err(|source| Error::InvalidArgument(source.to_string()))?;
    let seconds = author.time().map(|time| time.seconds).unwrap_or(0);
    Ok((author.name.to_str_lossy().into_owned(), seconds))
}

/// The rail-nav page families this crate exposes -- one variant per icon
/// in [`layout`]'s `.rail`, so a handler can name which rail item it
/// renders behind without `layout` re-deriving it from the request path
/// (the pre-redo `Tab` enum, carried through the workbench restructure:
/// the horizontal tab strip became the vertical icon rail, but the
/// "handler names its own section" contract is unchanged). The rail reads,
/// top to bottom: Dashboard (`Overview`), Code (`Files`), Commits, Reviews,
/// Issues, Agents (`docs/agent-sessions-plan.adoc`'s Phase 3), Threads
/// (`Comments`); then, past the spacer, Repo & governance
/// (`Meta`) and Account. Commits and Reviews are two rail items, not one,
/// even though every review still lives on its own commit's page
/// (`super::commits::reviews_section`) -- browsing history and judging a
/// specific commit are different reasons to be on this rail, so they get
/// their own icons (`super::reviews` is the read-only aggregate list; no
/// mutation route lives there). `Meta` covers five page families
/// ([`super::members`], [`super::effects`], [`super::toolchains`],
/// [`super::redactions`], [`super::inbox`]) behind one rail item and the
/// [`META_SECTIONS`] rail (see [`layout_meta`]) rather than an item each --
/// unrelated to the Commits/Reviews split above: those five are one page
/// family each with no reason to be found separately, unlike Commits and
/// Reviews. `None` highlights nothing at all, for a page that is not part
/// of any rail item's own section ([`super::search`]'s results page).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tab {
    Overview,
    Files,
    Commits,
    Reviews,
    Issues,
    Agents,
    Comments,
    Meta,
    Account,
    None,
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

/// The served repository's identity for the shell's `.wb-bar` top bar:
/// its directory name and, when `HEAD` resolves to a
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
/// through -- the workbench chrome (see [`layout_shell`]) around a
/// `main.content` column carrying the page's own `.page-header` title and
/// `body`. `active` names which rail item is current and `repo` the served
/// repository the top bar names. `identity` is the signing identity's
/// display label (see [`identity_label`]), rendered as the bar's
/// right-aligned `.id-chip` link to `/account` -- the same place the
/// rail's own account icon leads.
pub(crate) fn layout(
    repo: &RepoHeader,
    identity: &str,
    active: Tab,
    title: &str,
    body: Markup,
) -> Markup {
    layout_shell(
        repo,
        identity,
        active,
        title,
        html! {
            main.content {
                div.page-header { h1.page-title { (title) } }
                (body)
            }
        },
    )
}

/// One `.rail` item: an icon-only link into a page family, `title`-tipped
/// (the rail carries no text labels at all), highlighted when `tab` is the
/// page's own `active` section.
fn rail_link(active: Tab, tab: Tab, href: &str, title: &str, icon: &str) -> Markup {
    html! {
        a.active[active == tab] href=(href) title=(title) { (crate::assets::icon_use(icon)) }
    }
}

/// The workbench shell itself (the "Proposal C" chrome,
/// `docs/web-workbench-plan.adoc`): a `.wb` grid pairing the sticky icon
/// `.rail` (Dashboard / Code / Review / Issues / Agents / Threads, then
/// governance and account past the spacer -- see [`Tab`]'s own doc) with a `.wb-main`
/// column whose sticky `.wb-bar` top bar names the served repository and
/// its branch pill, carries the `.palette` search form (a plain GET to
/// `/search` for now -- the `⌘K` kbd is a hint at the palette phase, not
/// yet wired), and ends in the `.id-chip` identity link. `content` renders
/// below the bar as-is: [`layout`] passes the ordinary padded
/// `main.content` column, while a master-detail page passes its own
/// full-bleed `.split` instead.
pub(crate) fn layout_shell(
    repo: &RepoHeader,
    identity: &str,
    active: Tab,
    title: &str,
    content: Markup,
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
                script src="/ents.js" defer {}
            }
            body {
                (crate::assets::sprite())
                div.wb {
                    aside.rail {
                        span.nav-mark { "ge" }
                        (rail_link(active, Tab::Overview, "/", "Dashboard", "i-home"))
                        (rail_link(active, Tab::Files, "/files", "Code", "i-files"))
                        (rail_link(active, Tab::Commits, "/commits", "Commits", "i-commit"))
                        (rail_link(active, Tab::Reviews, "/reviews", "Reviews", "i-review"))
                        (rail_link(active, Tab::Issues, "/issues", "Issues", "i-issue"))
                        (rail_link(active, Tab::Agents, "/agents", "Agents", "i-agent"))
                        (rail_link(active, Tab::Comments, "/comments", "Threads", "i-comment"))
                        span.spacer {}
                        (rail_link(active, Tab::Meta, "/meta", "Repo & governance", "i-meta"))
                        (rail_link(active, Tab::Account, "/account", "Account", "i-person"))
                    }
                    div.wb-main {
                        div.wb-bar {
                            span.repo-path {
                                span.here { (repo.name) }
                                @if let Some(branch) = &repo.branch {
                                    span.branch { (crate::assets::icon_use("i-branch")) (branch) }
                                }
                            }
                            form.palette method="get" action="/search" {
                                (crate::assets::icon_use("i-search"))
                                input type="search" name="q" placeholder="Jump to file, commit, issue, member…" aria-label="Search";
                                kbd { "⌘K" }
                            }
                            a.id-chip href="/account" { (avatar(identity)) span { (identity) } }
                        }
                        (content)
                    }
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

/// Wrap `title`, `sidebar`, and `pane` in the master-detail split every
/// selection-heavy page family renders through ([`super::files`]'s tree
/// beside a blob, [`super::commits`]'s compact history beside a diff,
/// [`super::issues`]'s issue list beside an issue): the workbench chrome
/// ([`layout_shell`]) around a full-bleed `.split` grid -- a sticky
/// `nav.tree` sidebar on the left, a padded `main.pane` (carrying the
/// page's own `.page-header` title and `pane` body) on the right. Every
/// selection in the sidebar is a real URL and the sidebar always renders,
/// so the split stays SSR-friendly (`docs/web-workbench-plan.adoc`).
///
/// `path_title` marks `title` itself as a repository-relative path
/// (`super::files`'s tree/blob views, the only pages whose title is a path
/// rather than a name) so the title renders in `.page-title.path`'s
/// monospace, matching the `.crumbs` trail underneath it instead of
/// clashing with it in the ordinary heading font.
pub(crate) fn layout_split(
    repo: &RepoHeader,
    identity: &str,
    active: Tab,
    title: &str,
    path_title: bool,
    sidebar: Markup,
    pane: Markup,
) -> Markup {
    layout_shell(
        repo,
        identity,
        active,
        title,
        html! {
            div.split {
                nav.tree { (sidebar) }
                main.pane {
                    div.page-header { h1.page-title.path[path_title] { (title) } }
                    (pane)
                }
            }
        },
    )
}

/// The "open in editor" affordance rendered beside a code location: a
/// deep link into the serving user's own editor
/// ([`crate::editor::detected`]: `$ENTS_EDITOR`, then `$EDITOR`), its
/// icon naming which one. Renders nothing at all when no recognized
/// editor is configured -- the affordance is the escalation back to the
/// desk the reader came from (`docs/web-workbench-plan.adoc`), never a
/// dead link. The line-less deep link rides along as `data-editor-base`
/// so `ents.js` can retarget the blob header's affordance at the
/// currently selected line without rebuilding the URL client-side.
pub(crate) fn editor_open<O>(state: &AppState<O>, path: &str, line: Option<u64>) -> Markup {
    let Some(editor) = crate::editor::detected() else {
        return html! {};
    };
    let root = std::fs::canonicalize(&state.path).unwrap_or_else(|_io| state.path.clone());
    let abs = root.join(path);
    let name = path.rsplit('/').next().unwrap_or(path);
    let loc = match line {
        Some(line) => format!("{name}:{line}"),
        None => name.to_owned(),
    };
    html! {
        a.editor-open
            href=(editor.deep_link(&abs, line))
            data-editor-base=(editor.deep_link(&abs, None))
            title={ "Open in " (editor.label()) }
        {
            (crate::assets::icon_use("i-editor"))
            span.ed-loc { (loc) }
        }
    }
}

/// The signing identity's display label for [`layout`]'s `.id-chip`
/// (`roots.web-signing`) -- [`crate::identity::SigningIdentity::label`].
/// Every page reads this off `state` itself rather than `layout` reaching
/// into [`AppState`], so `layout` stays a pure function of the shell's own
/// chrome inputs (the same reason a [`Session`] is never threaded into it).
pub(crate) fn identity_label<O>(state: &AppState<O>) -> String {
    state.identity.label()
}

/// The design's initials avatar (`.avatar`): the first two characters of
/// `label` on the shared indigo→teal gradient, the same mark the top bar's
/// `.id-chip`, every comment card's author line, and an issue's assignee
/// list all render beside a name (README: initials on a gradient, no image
/// assets). Two characters because that is what the mock's own avatars show
/// ("ada.lang" → "ad"); a shorter label renders however many it has.
pub(crate) fn avatar(label: &str) -> Markup {
    let initials: String = label.chars().take(2).collect();
    html! {
        span.avatar { (initials) }
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
/// The author signature an attributed mutation carries
/// (`receive.attributed-author`): the session's signed-in member, stamped
/// with the current time, or `None` when the session holds no member --
/// every `Trusted` deployment, and a hosted request that somehow reached a
/// mutation anonymously (the auth middleware refuses those first). The
/// synthetic email domain is reserved (RFC 2606): a member record carries
/// no email of its own.
// @relation(receive.attributed-author, scope=function)
pub(crate) fn member_author(session: &Session) -> Option<gix::actor::Signature> {
    let member = session.member.as_ref()?;
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .try_into()
        .unwrap_or_default();
    Some(gix::actor::Signature {
        name: member.username.clone().into(),
        email: format!("{}@members.invalid", member.username).into(),
        time: gix::date::Time { seconds, offset: 0 },
    })
}

pub(crate) fn require_csrf(session: &Session, submitted: &str) -> Result<()> {
    if submitted == session.csrf {
        Ok(())
    } else {
        Err(Error::BadCsrf)
    }
}

/// A unix timestamp rendered as a relative "time ago" label, measured
/// against the current time -- hand-rolled from epoch seconds rather than
/// pulling in a date-formatting dependency, mirroring
/// `pre-redo:crates/git-ents-server/src/web/pages.rs`'s own `ago`/
/// `ago_seconds`. Shared by [`super::dashboard`]'s freshness strip and
/// [`super::commits`]'s list/show pages, the only places this crate names
/// a commit's age.
pub(crate) fn ago(then_seconds: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0);
    let secs = now.saturating_sub(then_seconds).max(0);
    let mins = secs.checked_div(60).unwrap_or(0);
    let hours = mins.checked_div(60).unwrap_or(0);
    let days = hours.checked_div(24).unwrap_or(0);
    if mins == 0 {
        "just now".to_owned()
    } else if hours == 0 {
        ago_plural(mins, "minute")
    } else if days == 0 {
        ago_plural(hours, "hour")
    } else if days < 30 {
        ago_plural(days, "day")
    } else if days < 365 {
        ago_plural(days.checked_div(30).unwrap_or(0), "month")
    } else {
        ago_plural(days.checked_div(365).unwrap_or(0), "year")
    }
}

/// Format `n` whole `unit`s with an "ago" suffix, pluralizing as needed --
/// [`ago`]'s own helper.
fn ago_plural(n: i64, unit: &str) -> String {
    if n == 1 {
        format!("1 {unit} ago")
    } else {
        format!("{n} {unit}s ago")
    }
}

/// The shared empty-state card (`ents.css`'s `.blankslate`): a short
/// title and one explanatory line, rendered instead of a bare list or a
/// header-only table when a page family has nothing to show yet. `line`
/// is markup, not text, so a page can point at its own create form or
/// link a next step ([`super::dashboard`]'s README pointer does the
/// same).
pub(crate) fn blankslate(title: &str, line: Markup) -> Markup {
    html! {
        div.card {
            div.blankslate {
                h2 { (title) }
                p { (line) }
            }
        }
    }
}

/// A `<datalist id="members">` of every enrolled username
/// (`refs/meta/member/*`), for forms whose text field names a member --
/// an issue's assignees completes by id in place; richer matching (by
/// key, fuzzy) stays with the palette. Best-effort: a ref-store read
/// failure renders an empty datalist rather than failing the page the
/// form sits on.
pub(crate) fn members_datalist<O>(state: &AppState<O>) -> Markup {
    let mut names = Vec::new();
    if let Ok(entries) = state.refs.iter_prefix("refs/meta/member/") {
        for (name, _tip) in entries.flatten() {
            let path = name.as_bstr().to_string();
            if let Some(username) = path.strip_prefix("refs/meta/member/") {
                names.push(username.to_owned());
            }
        }
    }
    html! {
        datalist id="members" {
            @for name in &names { option value=(name) {} }
        }
    }
}

/// The one-level breadcrumb trail every `/{id}` child page renders above
/// its own content -- "parent \u{203a} here", reusing the `.crumbs` markup
/// pattern [`super::files`]'s own multi-level path trail already renders
/// (same `nav.crumbs`/`span.sep`/`span.here` classes, so the stylesheet
/// needs no second breadcrumb rule). `parent` links back to the family's
/// list page at `parent_href`; `here` is the child's own display name, a
/// plain non-link "you are here" crumb.
pub(crate) fn child_crumbs(parent: &str, parent_href: &str, here: &str) -> Markup {
    html! {
        nav.crumbs {
            a href=(parent_href) { (parent) }
            span.sep { (crate::assets::icon_chevron()) }
            span.here { (here) }
        }
    }
}

/// A commit id shortened to seven hex characters for display -- mirrors
/// `pre-redo:crates/git-ents-server/src/web/pages.rs`'s own `short_oid`.
/// Falls back to the full id on the (practically unreachable) case that a
/// 7-character prefix is invalid for `oid`'s hash kind.
pub(crate) fn short_oid(oid: &ObjectId) -> String {
    gix_hash::Prefix::new(oid, 7).map_or_else(|_| oid.to_string(), |prefix| prefix.to_string())
}

/// Split a Scoped-Commits subject (`<scope>: <description>`,
/// scopedcommits.com) into its scope and description -- `None` when the
/// subject carries no `^[a-z-]+:` prefix, in which case the whole subject
/// renders unchipped. Shared by [`super::dashboard`]'s history strip and
/// [`super::commits`]'s commit rows, the two places a commit subject chips
/// its scope, so both split it the same way.
pub(crate) fn split_scope(subject: &str) -> Option<(&str, &str)> {
    let (scope, rest) = subject.split_once(':')?;
    if scope.is_empty() || !scope.chars().all(|c| c.is_ascii_lowercase() || c == '-') {
        return None;
    }
    Some((scope, rest.trim_start()))
}

/// The `.scope-c{n}` color class for `scope`: a stable hash of the scope
/// name onto the stylesheet's six deterministic chip colors (README's
/// "deterministic-color, fixed 52px" [`ScopeChip`]), so the same scope
/// always chips the same color across pages and requests. Shared with
/// [`split_scope`] by every page that renders a commit subject.
pub(crate) fn scope_class(scope: &str) -> String {
    let hash = scope.bytes().fold(0u32, |acc, byte| {
        acc.wrapping_mul(31).wrapping_add(u32::from(byte))
    });
    format!("scope-c{}", hash.checked_rem(6).unwrap_or(0))
}
