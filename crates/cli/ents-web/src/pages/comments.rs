//! `GET /comments`, `GET /comments/{id}`, `POST /comments`: a custom (not
//! generic) page family, per this crate's own top-level doc -- a
//! comment's anchor needs projection against a live working tree
//! (`anchor.projection`) to render meaningfully, which is exactly the
//! kind of domain-specific view `ents-forge`'s own `comment::show`
//! already returns structured data for, rather than a bare reflected
//! field list.
//!
//! [`for_path`]/[`comment_card`]/[`comments_section`] are this module's
//! second entry point: `crate::pages::files`'s blob view calls them to
//! render the comments anchored to the file it is showing -- inline,
//! interleaved at the anchored line, or in a below-the-blob section for
//! one with no current line to interleave at -- rather than duplicating
//! this module's own read-project-render pattern or its card markup.
//! [`for_commit`] is a third: `crate::pages::commits::show`'s own
//! "conversation" section, listing every comment whose anchor was captured
//! against that exact commit (`Anchor::commit`, not a projection onto any
//! revision -- a commit page shows what was written about that commit,
//! not merely reachable from it).

use std::sync::Arc;

use axum::Form;
use axum::extract::{Path, Query as PathQuery, State};
use axum::response::{IntoResponse, Redirect};
use ents_anchor::{Anchor, LineRange, Projection};
use ents_forge::comment;
use gix_hash::ObjectId;
use gix_object::{Find, Write};
use maud::{Markup, html};
use serde::Deserialize;

use crate::error::Result;
use crate::session::Session;
use crate::state::AppState;

/// The query parameters `GET /comments` accepts: `file`/`lines`/`rev`
/// prefill the add-comment form (e.g. a link from `crate::pages::files`'s
/// "comment on this file", or `crate::pages::commits::show`'s "comment on
/// this commit"), rather than changing what the page lists. All three
/// default to empty except `rev`, which defaults to `HEAD` exactly as the
/// add form always has -- an absent or nonsensical `file`/`lines` value
/// (neither is ever parsed here, only echoed back into the form) is
/// exactly as inert as an absent one.
#[derive(Debug, Deserialize)]
pub struct ListQuery {
    /// Pre-fills the add form's `path` field.
    #[serde(default)]
    file: String,
    /// Pre-fills the add form's `lines` field.
    #[serde(default)]
    lines: String,
    /// Pre-fills the add form's `rev` field; defaults to `HEAD`.
    #[serde(default = "default_rev_field")]
    rev: String,
}

impl Default for ListQuery {
    fn default() -> Self {
        Self {
            file: String::new(),
            lines: String::new(),
            rev: default_rev_field(),
        }
    }
}

/// `GET /comments?file=<path>&lines=<range>&rev=<rev>`.
///
/// # Errors
///
/// Propagates a ref-store or object read failure.
pub async fn list<O>(
    State(state): State<Arc<AppState<O>>>,
    axum::Extension(session): axum::Extension<Session>,
    PathQuery(query): PathQuery<ListQuery>,
) -> Result<maud::Markup>
where
    O: Find + Write + Send + 'static,
{
    let rows = comment::list(state.refs.as_ref(), &*state.objects())?;
    Ok(super::layout(
        &super::RepoHeader::from_state(&state),
        &super::identity_label(&state),
        super::Tab::Comments,
        "comments",
        html! {
            ul {
                @for (id, comment) in &rows {
                    li { a href=(format!("/comments/{id}")) { (id) } ": " (comment.body) }
                }
            }
            h2 { "add a comment" }
            (add_form(&query.rev, &session, &query.file, &query.lines))
        },
    ))
}

/// The query parameters `GET /comments/{id}` accepts: which revision to
/// project the anchor onto (defaults to `HEAD`).
#[derive(Debug, Deserialize)]
pub struct ShowQuery {
    /// The revision to project onto; defaults to `HEAD`.
    #[serde(default = "default_rev_field")]
    rev: String,
}

fn default_rev_field() -> String {
    "HEAD".to_owned()
}

/// `GET /comments/{id}?rev=...`: the comment's body, its anchor, and the
/// projection of that anchor onto `rev` (`anchor.projection`).
///
/// # Errors
///
/// [`crate::Error::Forge`] (wrapping [`ents_forge::Error::NotFound`]) if
/// `id` has no comment ref.
pub async fn show<O>(
    State(state): State<Arc<AppState<O>>>,
    Path(id): Path<String>,
    PathQuery(query): PathQuery<ShowQuery>,
) -> Result<maud::Markup>
where
    O: Find + Write + Send + 'static,
{
    let (comment, anchor, projection) = comment::show(
        state.refs.as_ref(),
        &*state.objects(),
        &state.path,
        &id,
        &query.rev,
    )?;
    Ok(super::layout(
        &super::RepoHeader::from_state(&state),
        &super::identity_label(&state),
        super::Tab::Comments,
        &id,
        html! {
            dl {
                dt { "path" } dd { (anchor.path) }
                dt { "lines" } dd { (format!("{:?}", anchor.lines)) }
                dt { "projection at " (query.rev) } dd { (format!("{projection:?}")) }
                dt { "body" } dd { (comment.body) }
            }
        },
    ))
}

/// The form fields `POST /comments` accepts.
#[derive(Debug, Deserialize)]
pub struct AddForm {
    /// The repository-relative path to anchor to.
    path: String,
    /// The comment's text.
    body: String,
    /// An optional `<start>[:<end>]` line range.
    #[serde(default)]
    lines: String,
    /// The revision to anchor against.
    rev: String,
    /// The per-session CSRF token (`roots.web-session`).
    csrf: String,
}

/// `POST /comments`: anchor `body` to `path` at `rev`, signed
/// (`roots.web-signing`) on behalf of the current session
/// (`roots.web-session`).
///
/// # Errors
///
/// [`crate::Error::BadCsrf`] if `form.csrf` does not match; otherwise
/// propagates [`ents_forge::comment::add`]'s own failures.
// @relation(roots.web-signing, roots.web-session, scope=function)
pub async fn add<O>(
    State(state): State<Arc<AppState<O>>>,
    axum::Extension(session): axum::Extension<Session>,
    Form(form): Form<AddForm>,
) -> Result<impl IntoResponse>
where
    O: Find + Write + Send + 'static,
{
    super::require_csrf(&session, &form.csrf)?;
    let lines = (!form.lines.trim().is_empty()).then(|| form.lines.trim().to_owned());

    let identity = state.identity.as_ref();
    let (id, outcome) = comment::add(
        state.refs.as_ref(),
        &*state.objects(),
        state.events.as_ref(),
        &state.path,
        &form.path,
        form.body,
        lines,
        &form.rev,
        &crate::receive_identity!(identity),
        state.mode,
    )?;
    crate::error::outcome_to_result(outcome)?;
    Ok(Redirect::to(&format!("/comments/{id}")))
}

/// The add-comment form, its `path`/`lines` fields pre-filled from
/// [`ListQuery`] when `list` was reached with `?file=`/`?lines=` (e.g.
/// `crate::pages::files`'s "comment on this file" link) -- maud escapes
/// both into the `value` attribute the same as any other interpolation,
/// so neither can break out of the form markup, and an empty prefill
/// renders exactly as the unfilled field always did.
fn add_form(
    default_rev: &str,
    session: &Session,
    prefill_path: &str,
    prefill_lines: &str,
) -> maud::Markup {
    html! {
        form method="post" action="/comments" {
            (super::csrf_input(session))
            label { "path" input type="text" name="path" value=(prefill_path); }
            label { "rev" input type="text" name="rev" value=(default_rev); }
            label { "lines" input type="text" name="lines" value=(prefill_lines); }
            label { "body" textarea name="body" {} }
            button type="submit" { "comment" }
        }
    }
}

/// One comment as `crate::pages::files`'s blob view shows it: who wrote it
/// and when ([`super::ago`]), where its anchor lands (a path plus a line
/// range, when it has one to interleave at -- [`comment_card`]'s own doc),
/// and its body rendered as AsciiDoc ([`crate::asciidoc`], this crate's
/// default prose treatment for text with no filename of its own to infer a
/// MIME type from). Mirrors `pre-redo:crates/git-ents-server/src/web/pages.rs`'s
/// own `FileComment`, salvaged per this crate's PORT-and-reverify policy:
/// author/timestamp there came from `git_comment::provenance`'s shell-out,
/// here from [`super::commit_authorship`] reading the comment ref's own tip
/// commit through `gix_object::Find`.
pub(crate) struct FileComment {
    /// The comment ref's own tip commit's author display name
    /// (`model.comment`: a comment stores no author field of its own).
    pub(crate) author: String,
    /// [`super::ago`] renders this against the current time.
    pub(crate) seconds: i64,
    /// The repository-relative path this comment's anchor lands on: the
    /// file [`for_path`] was called for (it filters to exactly that path),
    /// or the anchor's own recorded path for [`for_commit`] (a commit's
    /// conversation spans every file the commit touched, so there is no
    /// single implied path the way a blob view has one).
    pub(crate) path: String,
    /// The anchored range as it lands on the displayed file at `HEAD`, or
    /// `None` for a whole-file anchor or an outdated projection -- either
    /// way, nothing for [`crate::pages::files`]'s blob view to interleave
    /// the card after, so it renders in a below-the-blob section instead.
    /// [`for_commit`] always uses the anchor's own recorded range as-is
    /// (never projected), since a commit's conversation is about that
    /// commit specifically, not about `HEAD`.
    pub(crate) lines: Option<LineRange>,
    /// Set when [`ents_anchor::project`] reports
    /// [`Projection::Outdated`]: the anchored lines themselves were
    /// edited, so no line link is shown, only the marker -- the comment
    /// itself is never dropped from the page. Always `false` for
    /// [`for_commit`]'s own rows: "outdated" is a projection-onto-`HEAD`
    /// concept, and a commit page shows the anchor exactly as captured.
    pub(crate) outdated: bool,
    /// The body, rendered as AsciiDoc ([`crate::asciidoc::to_html`]),
    /// falling back to escaped plain text on a render failure -- a file
    /// view degrades, it never 500s over one unparsable comment.
    pub(crate) body: Markup,
}

/// Every comment whose anchor projects onto `path` at `HEAD` in `repo` --
/// [`crate::pages::files`]'s own read of this domain, built on the same
/// [`comment::list`] read [`list`] itself uses and the same
/// [`ents_anchor::project`] call [`show`] itself uses, rather than a third
/// way to read a comment. Best effort throughout: a comment whose anchor
/// or body fails to read, parse, or project is skipped from this file's
/// own view only -- it still shows up on `GET /comments` and its own `GET
/// /comments/{id}` page -- and a projection landing anywhere other than
/// `path` (moved elsewhere, or deleted) is likewise not this file's
/// comment to show. A projection that still lands at `path` but comes
/// back [`Projection::Outdated`] is the one case this function keeps and
/// flags (`outdated: true`) rather than skips: the anchored lines
/// changed, not the comment's relevance to this file.
pub(crate) fn for_path<O: Find + Write>(
    state: &AppState<O>,
    repo: &gix::Repository,
    path: &str,
) -> Vec<FileComment> {
    let Ok(rows) = comment::list(state.refs.as_ref(), &*state.objects()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (id, comment) in rows {
        let Ok(anchor) =
            facet_git_tree::deserialize::<Anchor>(&comment.anchor.oid(), &*state.objects())
        else {
            continue;
        };
        let Ok(projection) = ents_anchor::project(repo, &anchor, "HEAD") else {
            continue;
        };
        let (landed, lines, outdated) = match projection {
            Projection::Current => (anchor.path.clone(), anchor.lines, false),
            Projection::Relocated { path, lines } => (path, lines, false),
            Projection::Outdated { path } => (path, None, true),
            Projection::Deleted => continue,
        };
        if landed != path {
            continue;
        }
        let Ok(ref_name) = ents_model::namespace::comment_ref(&id) else {
            continue;
        };
        let Some(tip) = state.refs.get(ref_name.as_ref()).ok().flatten() else {
            continue;
        };
        let Ok((author, seconds)) = super::commit_authorship(&*state.objects(), tip) else {
            continue;
        };
        let body = crate::asciidoc::to_html(&comment.body)
            .unwrap_or_else(|_| html! { p { (comment.body) } });
        out.push(FileComment {
            author,
            seconds,
            path: landed,
            lines,
            outdated,
            body,
        });
    }
    out
}

/// Every comment whose anchor was captured against `commit_id` exactly --
/// `crate::pages::commits::show`'s own "conversation" section. Filtered by
/// [`Anchor::commit`] (the resolved commit oid `ents_anchor::capture`
/// records at write time), not by projecting onto any revision the way
/// [`for_path`] does: a commit page shows what was written about that
/// commit specifically, so an anchor is read here exactly as captured,
/// never re-projected (`lines`/`path` mirror [`Anchor::lines`]/
/// [`Anchor::path`] verbatim, `outdated` is always `false`). Best effort
/// throughout, mirroring [`for_path`]'s own stance: a comment whose anchor
/// or body fails to read or parse is skipped from this commit's own view
/// only.
pub(crate) fn for_commit<O: Find + Write>(
    state: &AppState<O>,
    commit_id: ObjectId,
) -> Vec<FileComment> {
    let Ok(rows) = comment::list(state.refs.as_ref(), &*state.objects()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (id, comment) in rows {
        let Ok(anchor) =
            facet_git_tree::deserialize::<Anchor>(&comment.anchor.oid(), &*state.objects())
        else {
            continue;
        };
        if anchor.commit() != commit_id {
            continue;
        }
        let Ok(ref_name) = ents_model::namespace::comment_ref(&id) else {
            continue;
        };
        let Some(tip) = state.refs.get(ref_name.as_ref()).ok().flatten() else {
            continue;
        };
        let Ok((author, seconds)) = super::commit_authorship(&*state.objects(), tip) else {
            continue;
        };
        let body = crate::asciidoc::to_html(&comment.body)
            .unwrap_or_else(|_| html! { p { (comment.body) } });
        out.push(FileComment {
            author,
            seconds,
            path: anchor.path.clone(),
            lines: anchor.lines,
            outdated: false,
            body,
        });
    }
    out
}

/// Where a [`FileComment`]'s line-range link points -- [`comment_card`]'s
/// own mode switch between the pages that render one.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum LinkMode {
    /// The comment renders on the same page as the file it anchors to
    /// (`crate::pages::files`'s blob view, whether interleaved at its own
    /// line or in the below-the-blob section): the link is an in-page
    /// fragment (`#L<n>`), labeled just the line range -- the path is
    /// implied by the page itself.
    SameFile,
    /// The comment renders on a page about something else
    /// (`crate::pages::commits::show`'s "conversation" section, which can
    /// span several files): the link crosses into the file browser
    /// (`/files/<path>#L<n>`), labeled with the path so the reader knows
    /// where it lands.
    CrossFile,
}

/// One comment's card: author, [`super::ago`] time, its line-range link
/// (per `link`'s [`LinkMode`]) or the muted `outdated` marker, and its body
/// -- the single rendering every comment-showing page in this crate shares
/// ([`comments_section`]'s below-the-blob list, `crate::pages::files`'s own
/// inline-interleaved rows, `crate::pages::commits::show`'s "conversation"
/// section), so a comment's markup is defined in exactly one place. `index`
/// names this card's `id="comment-<index>"` anchor, stable within
/// whichever page rendered it (not a global id): `crate::pages::files`'s
/// crumbs "N comments" jump link targets `comment-0`, the first comment in
/// display order, regardless of whether it landed inline or below the
/// blob.
pub(crate) fn comment_card(index: usize, comment: &FileComment, link: LinkMode) -> Markup {
    html! {
        div.card id={ "comment-" (index) } {
            div.comment-meta {
                span.author { (comment.author) }
                span { (super::ago(comment.seconds)) }
                @if let Some(range) = comment.lines {
                    @match link {
                        LinkMode::SameFile => {
                            a href={ "#L" (range.start) } {
                                @if range.start == range.end { "line " (range.start) }
                                @else { "lines " (range.start) "-" (range.end) }
                            }
                        }
                        LinkMode::CrossFile => {
                            a href={ "/files/" (comment.path) "#L" (range.start) } {
                                (comment.path) "#L" (range.start)
                                @if range.start != range.end { "-" (range.end) }
                            }
                        }
                    }
                }
                @if comment.outdated {
                    span.outdated { "outdated" }
                }
            }
            div.doc-body { (comment.body) }
        }
    }
}

/// The comment cards under a blob view (a rendered document, a binary
/// placeholder, or -- for a raw-source view -- the ones with no current
/// line range to interleave at; see `crate::pages::files::source_view`),
/// one [`comment_card`] per entry (in [`LinkMode::SameFile`]). Renders
/// nothing at all -- not even an empty container -- when `comments` is
/// empty, so a file with no comments carries no extra markup
/// (`crate::pages::files`'s own blob view calls this unconditionally
/// rather than checking first).
pub(crate) fn comments_section(comments: &[FileComment]) -> Markup {
    html! {
        @for (index, comment) in comments.iter().enumerate() {
            (comment_card(index, comment, LinkMode::SameFile))
        }
    }
}
