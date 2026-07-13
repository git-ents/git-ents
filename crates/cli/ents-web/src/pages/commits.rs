//! `GET /commits`, `GET /commit/{oid}`: a read-only commit history and
//! per-commit unified diff over `HEAD` -- a view of the code, not a tab of
//! its own (both routes render with [`super::Tab::Files`] active; see
//! [`super`]'s own doc), reached from [`super::files`]'s "history" link.
//!
//! Reads go through `gix`'s high-level `Repository`/`Commit`/`Tree` types,
//! opened fresh per request from `state.path`, exactly as
//! [`super::files`]/[`super::dashboard`] browse `HEAD` -- `facet-git-tree`'s
//! typed-tree convention is for meta-ref entities, not browsing arbitrary
//! repository history. The unified diff itself is built directly on top of
//! `gix::diff::blob` (`gix_diff`'s own re-export through the `gix`
//! facade): [`gix::diff::blob::InternedInput`] interns each side's lines,
//! [`gix::diff::blob::diff_with_slider_heuristics`] computes the hunks, and
//! [`gix::diff::blob::unified_diff::ConsumeBinaryHunk`] renders them as the
//! same textual unified-diff format `git diff` itself produces, which
//! [`diff_class`] then colorizes line by line -- no new dependency, since
//! `gix`'s default features already enable `blob-diff`.
//!
//! `GET /commit/{oid}` also lists a "conversation": every comment whose
//! anchor was captured against that exact commit
//! (`crate::pages::comments::for_commit`), rendered below the diff via the
//! same [`crate::pages::comments::comment_card`] a blob view uses, each
//! naming its `path#lines` and linking into `crate::pages::files`'s own
//! `#L<n>` gutter. A "comment on this commit" link beside the parents list
//! reaches `crate::pages::comments::list`'s add form with `rev` prefilled
//! to this commit's own oid.

use std::sync::Arc;

use axum::Form;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Redirect};
use gix::bstr::ByteSlice as _;
use gix::diff::blob::unified_diff::{ConsumeBinaryHunk, ContextSize};
use gix::diff::blob::{Algorithm, InternedInput, UnifiedDiff, diff_with_slider_heuristics};
use gix::object::tree::diff::Change;
use gix_hash::ObjectId;
use gix_object::{Find, Write};
use maud::{Markup, html};
use serde::Deserialize;

use crate::error::{Error, Result};
use crate::session::Session;
use crate::state::AppState;

/// One page of `GET /commits`.
const PAGE_SIZE: usize = 50;

/// The largest total diff rendered in full on `GET /commit/{oid}` -- past
/// it, reading every changed blob into memory and diffing it would be
/// unbounded, so the page shows a truncation notice instead (mirrors
/// `pre-redo:crates/git-ents-server/src/web/pages.rs`'s own
/// `MAX_RENDER_BYTES`, at this page's own, smaller budget).
const MAX_DIFF_BYTES: usize = 1024 * 1024;

/// The query parameters `GET /commits` accepts.
#[derive(Debug, Deserialize)]
pub struct ListQuery {
    /// Continue the walk just past this previously shown commit (the
    /// "older" link) -- omitted for the first page.
    from: Option<String>,
}

/// One row of `GET /commits`.
struct CommitRow {
    /// The full commit id, the `/commit/{oid}` link target.
    oid: ObjectId,
    /// [`super::short_oid`] of `oid`, the row's displayed, mono id.
    short: String,
    /// The commit message's title line.
    subject: String,
    /// The commit author's display name.
    author: String,
    /// [`super::ago`] of the commit author's time.
    ago: String,
}

/// `GET /commits`: the repository's commit history, newest first, 50 per
/// page.
///
/// # Errors
///
/// Never fails on an unopenable repository or an unborn `HEAD` -- both
/// degrade to a blankslate.
pub async fn list<O>(
    State(state): State<Arc<AppState<O>>>,
    Query(params): Query<ListQuery>,
) -> Result<Markup>
where
    O: Find + Write + Send + 'static,
{
    let (rows, older) = commit_rows(&state, params.from.as_deref());
    Ok(super::layout(
        &super::RepoHeader::from_state(&state),
        &super::identity_label(&state),
        super::Tab::Files,
        "commits",
        html! {
            @if rows.is_empty() {
                (blankslate())
            } @else {
                div.card {
                    div.card-header { "commits" }
                    table.entity-list.commits-table {
                        thead {
                            tr { th { "commit" } th { "subject" } th { "author" } th { "when" } }
                        }
                        tbody {
                            @for row in &rows {
                                tr {
                                    td { a href={ "/commit/" (row.oid) } { code { (row.short) } } }
                                    td { (row.subject) }
                                    td { (row.author) }
                                    td { (row.ago) }
                                }
                            }
                        }
                    }
                }
                @if let Some(from) = older {
                    nav.crumbs {
                        a href={ "/commits?from=" (from) } { "older" }
                    }
                }
            }
        },
    ))
}

/// Up to [`PAGE_SIZE`] rows starting at `from` (or `HEAD` when `from` is
/// `None`), newest first, plus the oid to continue from for an "older"
/// link when more commits remain. Best-effort: an unopenable repository,
/// an unborn `HEAD`, or an unparsable/unresolvable `from` all degrade to
/// an empty page rather than an error.
fn commit_rows<O>(state: &AppState<O>, from: Option<&str>) -> (Vec<CommitRow>, Option<String>) {
    let Ok(repo) = gix::open(&state.path) else {
        return (Vec::new(), None);
    };
    let continuing = from.and_then(|hex| ObjectId::from_hex(hex.as_bytes()).ok());
    let tip = match continuing {
        Some(oid) => oid,
        None => {
            let Ok(head) = repo.head_id() else {
                return (Vec::new(), None);
            };
            head.detach()
        }
    };
    let Ok(walk) = repo
        .rev_walk([tip])
        .sorting(gix::revision::walk::Sorting::ByCommitTime(
            gix::traverse::commit::simple::CommitTimeOrder::NewestFirst,
        ))
        .all()
    else {
        return (Vec::new(), None);
    };

    let skip = if continuing.is_some() { 1 } else { 0 };
    let mut rows: Vec<CommitRow> = Vec::new();
    let mut has_more = false;
    for info in walk.skip(skip) {
        let Ok(info) = info else { break };
        if rows.len() == PAGE_SIZE {
            has_more = true;
            break;
        }
        let Ok(commit) = info.object() else { continue };
        let Ok(message) = commit.message() else {
            continue;
        };
        let Ok(author) = commit.author() else {
            continue;
        };
        let seconds = author.time().map(|time| time.seconds).unwrap_or(0);
        let oid = info.id().detach();
        rows.push(CommitRow {
            oid,
            short: super::short_oid(&oid),
            subject: message.title.to_str_lossy().into_owned(),
            author: author.name.to_str_lossy().into_owned(),
            ago: super::ago(seconds),
        });
    }
    let older = has_more
        .then(|| rows.last().map(|row| row.oid.to_string()))
        .flatten();
    (rows, older)
}

/// The empty-history placeholder: an unborn `HEAD`, or a repository this
/// page could not open at all.
fn blankslate() -> Markup {
    html! {
        div.card {
            div.blankslate {
                h2 { "No commits yet" }
                p { "This repository has no history to show." }
            }
        }
    }
}

/// `GET /commit/{oid}`: a single commit's full message, metadata, and a
/// unified diff against its first parent (or the empty tree, for a root
/// commit).
///
/// # Errors
///
/// [`Error::NotFound`] if `oid` is not a well-formed object id or does not
/// name a commit in the served repository.
pub async fn show<O>(
    State(state): State<Arc<AppState<O>>>,
    axum::Extension(session): axum::Extension<Session>,
    Path(oid): Path<String>,
) -> Result<Markup>
where
    O: Find + Write + Send + 'static,
{
    let object_id = parse_oid(&oid)?;
    let repo = gix::open(&state.path).map_err(|source| Error::Repo(source.to_string()))?;
    let commit = repo
        .find_commit(object_id)
        .ok()
        .ok_or_else(|| Error::NotFound { what: oid.clone() })?;
    let message = commit
        .message()
        .map_err(|source| Error::Repo(source.to_string()))?;
    let subject = message.title.to_str_lossy().into_owned();
    let body = message
        .body
        .map(|body| body.to_str_lossy().into_owned())
        .filter(|body| !body.is_empty());
    let author = commit
        .author()
        .map_err(|source| Error::Repo(source.to_string()))?;
    let author_name = author.name.to_str_lossy().into_owned();
    let ago = author.time().map(|time| super::ago(time.seconds)).ok();
    let parents: Vec<ObjectId> = commit.parent_ids().map(|id| id.detach()).collect();
    let new_tree = commit
        .tree()
        .map_err(|source| Error::Repo(source.to_string()))?;

    let old_tree = match parents.first() {
        Some(parent) => Some(
            repo.find_commit(*parent)
                .map_err(|source| Error::Repo(source.to_string()))?
                .tree()
                .map_err(|source| Error::Repo(source.to_string()))?,
        ),
        None => None,
    };
    let empty_tree = repo.empty_tree();
    let old_tree_ref = old_tree.as_ref().unwrap_or(&empty_tree);
    let (diff, truncated) = diff_sections(&repo, old_tree_ref, &new_tree);
    let comments = super::comments::for_commit(&state, object_id);
    let reviews = reviews_section(&state, &session, object_id, &oid);

    Ok(super::layout(
        &super::RepoHeader::from_state(&state),
        &super::identity_label(&state),
        super::Tab::Files,
        &subject,
        html! {
            div.card {
                div.card-header { "commit " code { (super::short_oid(&object_id)) } }
                div.commit {
                    div.commit-subject { (subject) }
                    @if let Some(body) = &body {
                        div.commit-msg { (body) }
                    }
                    div.commit-meta {
                        (author_name)
                        @if let Some(ago) = &ago { " \u{b7} " (ago) }
                    }
                    div.commit-meta {
                        "tree " a href={ "/files" } { "browse at HEAD" }
                        @if !parents.is_empty() {
                            " \u{b7} parents: "
                            @for (index, parent) in parents.iter().enumerate() {
                                @if index > 0 { ", " }
                                a href={ "/commit/" (parent) } { code { (super::short_oid(parent)) } }
                            }
                        } @else {
                            " \u{b7} root commit"
                        }
                        " \u{b7} "
                        a href={ "/comments?rev=" (object_id) } { "comment on this commit" }
                    }
                }
            }
            (reviews)
            (diff)
            @if truncated {
                div.card { div.binary { "Diff truncated (over " (MAX_DIFF_BYTES / (1024 * 1024)) " MiB)." } }
            }
            @if !comments.is_empty() {
                h2 { "conversation" }
                @for (index, comment) in comments.iter().enumerate() {
                    (super::comments::comment_card(index, comment, super::comments::LinkMode::CrossFile))
                }
            }
        },
    ))
}

/// Every review targeting `commit_id` (`ents_forge::review::list` filtered
/// to this commit, `model.review`), each rendering its verdict prominently,
/// its body as AsciiDoc, and its reviewer (from the review ref's own tip
/// commit chain, `meta-ref.trailers` -- a review stores no author field),
/// followed by its discussion: the comments naming `reviews/<id>` as their
/// context (`ents_forge::comment::thread`, `model.comment-context`),
/// rendered through the same shared `super::comments::thread_section` an
/// issue's thread uses. A "start a review" form closes the section
/// (`POST /commit/{oid}/review`). Best effort: a review whose ref cannot be
/// listed degrades to just the start form rather than failing the page.
fn reviews_section<O: Find + Write>(
    state: &AppState<O>,
    session: &Session,
    commit_id: ObjectId,
    oid: &str,
) -> Markup {
    let reviews = ents_forge::review::list(
        state.refs.as_ref(),
        &*state.objects(),
        &state.path,
        Some(&commit_id.to_string()),
    )
    .unwrap_or_default();
    let return_to = format!("/commit/{oid}");
    html! {
        h2 { "reviews" }
        @for (id, review) in &reviews {
            div.card {
                div.comment-meta {
                    span.verdict { (review.verdict) }
                    @let reviewer = ents_model::namespace::review_ref(id)
                        .ok()
                        .and_then(|ref_name| state.refs.get(ref_name.as_ref()).ok().flatten())
                        .and_then(|tip| super::commit_authorship(&*state.objects(), tip).ok());
                    @if let Some((author, seconds)) = &reviewer {
                        span.author { (author) }
                        span { (super::ago(*seconds)) }
                    }
                }
                div.doc-body {
                    (crate::asciidoc::to_html(&review.body).unwrap_or_else(|_| html! { p { (review.body) } }))
                }
                @let thread = ents_forge::comment::thread(
                    state.refs.as_ref(),
                    &*state.objects(),
                    &format!("reviews/{id}"),
                ).unwrap_or_default();
                (super::comments::thread_section(state, session, &thread, &return_to))
                (review_comment_form(session, id, &return_to))
            }
        }
        (start_review_form(session, oid))
    }
}

/// The comment-on-this-review form (`POST /reviews/{id}/comment`): a
/// contextual comment naming `reviews/<id>` (`model.comment-context`), so a
/// review's discussion can start from the web and not only the CLI or lens.
fn review_comment_form(session: &Session, id: &str, return_to: &str) -> Markup {
    html! {
        form method="post" action=(format!("/reviews/{id}/comment")) {
            (super::csrf_input(session))
            input type="hidden" name="return_to" value=(return_to);
            label { "comment on this review" textarea name="body" {} }
            button type="submit" { "comment" }
        }
    }
}

/// The form fields `POST /reviews/{id}/comment` accepts.
#[derive(Debug, Deserialize)]
pub struct ReviewCommentForm {
    /// The comment's body text.
    body: String,
    /// The per-session CSRF token (`roots.web-session`).
    csrf: String,
    /// Where to send the browser back to -- the commit page rendering the
    /// review; honored only when it is a same-origin path.
    #[serde(default)]
    return_to: String,
}

/// `POST /reviews/{id}/comment`: a comment naming `reviews/<id>` as its
/// context (`model.comment-context`) -- an ordinary
/// [`ents_forge::comment::add`], contextual and unanchored, joining the
/// review's discussion thread the moment it lands.
///
/// # Errors
///
/// [`Error::BadCsrf`] if `form.csrf` does not match; otherwise propagates
/// [`ents_forge::comment::add`]'s own failures.
// @relation(model.comment-context, roots.web-signing, roots.web-session, scope=function)
pub async fn review_comment<O>(
    State(state): State<Arc<AppState<O>>>,
    axum::Extension(session): axum::Extension<Session>,
    Path(id): Path<String>,
    Form(form): Form<ReviewCommentForm>,
) -> Result<impl IntoResponse>
where
    O: Find + Write + Send + 'static,
{
    super::require_csrf(&session, &form.csrf)?;
    let identity = state.identity.as_ref();
    let new = ents_forge::comment::NewComment {
        body: form.body,
        path: None,
        lines: None,
        rev: "HEAD".to_owned(),
        worktree: false,
        context: Some(format!("reviews/{id}")),
        parent: None,
    };
    let (_comment_id, outcome) = ents_forge::comment::add(
        state.refs.as_ref(),
        &*state.objects(),
        state.events.as_ref(),
        &state.path,
        new,
        &crate::receive_identity!(identity),
        state.mode,
    )?;
    crate::error::outcome_to_result(outcome)?;
    let target = if form.return_to.starts_with('/') {
        form.return_to
    } else {
        "/commits".to_owned()
    };
    Ok(Redirect::to(&target))
}

/// The start-a-review form (`POST /commit/{oid}/review`): a verdict
/// (`approve`, `request-changes`, or any custom value -- `model.review`
/// makes these conventions, not an enum) and a body.
fn start_review_form(session: &Session, oid: &str) -> Markup {
    html! {
        form method="post" action=(format!("/commit/{oid}/review")) {
            (super::csrf_input(session))
            label { "verdict" input type="text" name="verdict" value="approve"; }
            label { "body" textarea name="body" {} }
            button type="submit" { "start a review" }
        }
    }
}

/// The form fields `POST /commit/{oid}/review` accepts.
#[derive(Debug, Deserialize)]
pub struct ReviewForm {
    /// The review's verdict.
    verdict: String,
    /// The review's body text.
    #[serde(default)]
    body: String,
    /// The per-session CSRF token (`roots.web-session`).
    csrf: String,
}

/// `POST /commit/{oid}/review`: review the commit at `oid`
/// (`ents_forge::review::new`), which writes both the review's entity ref
/// and its retention pin (`model.review`, `model.review-pin`) -- the web is
/// another caller of that one library func, never a second review or
/// pin-writing path. Signed (`roots.web-signing`) on behalf of the current
/// session (`roots.web-session`).
///
/// # Errors
///
/// [`Error::BadCsrf`] if `form.csrf` does not match; otherwise propagates
/// [`ents_forge::review::new`]'s own failures (including an unresolvable
/// target commit).
// @relation(model.review, model.review-pin, roots.web-signing, roots.web-session, scope=function)
pub async fn review<O>(
    State(state): State<Arc<AppState<O>>>,
    axum::Extension(session): axum::Extension<Session>,
    Path(oid): Path<String>,
    Form(form): Form<ReviewForm>,
) -> Result<impl IntoResponse>
where
    O: Find + Write + Send + 'static,
{
    super::require_csrf(&session, &form.csrf)?;
    let identity = state.identity.as_ref();
    let new = ents_forge::review::NewReview {
        target: oid.clone(),
        verdict: form.verdict,
        body: form.body,
    };
    let (_id, outcome) = ents_forge::review::new(
        state.refs.as_ref(),
        &*state.objects(),
        state.events.as_ref(),
        &state.path,
        new,
        &crate::receive_identity!(identity),
        state.mode,
    )?;
    crate::error::outcome_to_result(outcome)?;
    Ok(Redirect::to(&format!("/commit/{oid}")))
}

/// Validate `text` as a full, well-formed object id -- hex characters only,
/// at the exact length the served repository's hash kind expects (this
/// page does not resolve abbreviated prefixes; [`super::commits::list`]'s
/// own links always carry the full id).
///
/// # Errors
///
/// [`Error::NotFound`] if `text` is empty, not hex, or the wrong length.
fn parse_oid(text: &str) -> Result<ObjectId> {
    if text.is_empty() || text.len() > 64 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::NotFound {
            what: text.to_owned(),
        });
    }
    ObjectId::from_hex(text.as_bytes())
        .ok()
        .ok_or_else(|| Error::NotFound {
            what: text.to_owned(),
        })
}

/// One `.diff` section per changed file between `old_tree` and `new_tree`,
/// plus whether the total rendered bytes exceeded [`MAX_DIFF_BYTES`] (in
/// which case the caller shows a truncation notice). Best-effort: a change
/// this function cannot read renders as a bare file header with no hunks
/// rather than failing the whole page.
fn diff_sections(
    repo: &gix::Repository,
    old_tree: &gix::Tree<'_>,
    new_tree: &gix::Tree<'_>,
) -> (Markup, bool) {
    let Ok(mut platform) = old_tree.changes() else {
        return (html! {}, false);
    };
    let mut sections = Vec::new();
    let mut total: usize = 0;
    let mut truncated = false;
    let _outcome = platform.for_each_to_obtain_tree(new_tree, |change| {
        if truncated {
            return Ok::<_, std::convert::Infallible>(std::ops::ControlFlow::Break(()));
        }
        let (section, bytes) = render_change(repo, &change);
        total = total.saturating_add(bytes);
        sections.push(section);
        if total > MAX_DIFF_BYTES {
            truncated = true;
        }
        Ok(std::ops::ControlFlow::Continue(()))
    });
    (html! { @for section in &sections { (section) } }, truncated)
}

/// One changed file's `.diff` section: a `.file`-classed header naming the
/// path (and, on a rename, the old path it moved from), followed by either
/// a `.meta`-classed "binary file changed" notice or the colorized unified
/// diff between its old and new blob content. Returns the section's
/// rendered byte cost, so [`diff_sections`] can track the page's overall
/// budget.
fn render_change(repo: &gix::Repository, change: &Change<'_, '_, '_>) -> (Markup, usize) {
    let (old_id, new_id, path, rename_from) = match *change {
        Change::Addition { location, id, .. } => (
            None,
            Some(id.detach()),
            location.to_str_lossy().into_owned(),
            None,
        ),
        Change::Deletion { location, id, .. } => (
            Some(id.detach()),
            None,
            location.to_str_lossy().into_owned(),
            None,
        ),
        Change::Modification {
            location,
            previous_id,
            id,
            ..
        } => (
            Some(previous_id.detach()),
            Some(id.detach()),
            location.to_str_lossy().into_owned(),
            None,
        ),
        Change::Rewrite {
            location,
            source_location,
            source_id,
            id,
            ..
        } => (
            Some(source_id.detach()),
            Some(id.detach()),
            location.to_str_lossy().into_owned(),
            Some(source_location.to_str_lossy().into_owned()),
        ),
    };

    let old_bytes = old_id.and_then(|id| blob_bytes(repo, id));
    let new_bytes = new_id.and_then(|id| blob_bytes(repo, id));
    let cost = old_bytes
        .as_ref()
        .map_or(0, Vec::len)
        .saturating_add(new_bytes.as_ref().map_or(0, Vec::len));
    let binary =
        old_bytes.as_deref().is_some_and(is_binary) || new_bytes.as_deref().is_some_and(is_binary);

    let header = html! {
        span.ln.file {
            @if let Some(from) = &rename_from { (from) " \u{2192} " }
            (path)
            "\n"
        }
    };
    let body = if binary {
        html! { span.ln.meta { "Binary file changed.\n" } }
    } else {
        let old_text = old_bytes.as_deref().map_or_else(String::new, |bytes| {
            String::from_utf8_lossy(bytes).into_owned()
        });
        let new_text = new_bytes.as_deref().map_or_else(String::new, |bytes| {
            String::from_utf8_lossy(bytes).into_owned()
        });
        unified_diff(&old_text, &new_text)
    };
    (html! { div.diff { (header) (body) } }, cost)
}

/// `id`'s blob content, or `None` when it cannot be read as a blob (a
/// submodule/gitlink entry, or a read failure) -- best-effort, mirroring
/// [`diff_sections`]'s own degrade-don't-fail stance.
fn blob_bytes(repo: &gix::Repository, id: ObjectId) -> Option<Vec<u8>> {
    Some(
        repo.find_object(id)
            .ok()?
            .try_into_blob()
            .ok()?
            .data
            .clone(),
    )
}

/// Whether `bytes` looks like binary content (a NUL byte in the leading
/// chunk, the same heuristic [`super::files::is_binary`] and pre-redo's own
/// `is_binary` use).
fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8000).any(|b| *b == 0)
}

/// `old_text` and `new_text` rendered as a colorized unified diff: each
/// hunk built via [`gix::diff::blob::InternedInput`] and
/// [`diff_with_slider_heuristics`], then rendered to the textual unified
/// diff format through [`ConsumeBinaryHunk`] and colorized line by line via
/// [`diff_class`] -- mirrors `pre-redo:.../pages.rs`'s own `diff_view`,
/// its `git diff`-shelled-out patch text replaced with this page's own
/// `gix`-computed one.
fn unified_diff(old_text: &str, new_text: &str) -> Markup {
    if old_text == new_text {
        return html! {};
    }
    let input = InternedInput::new(old_text, new_text);
    let diff = diff_with_slider_heuristics(Algorithm::Histogram, &input);
    let Ok(patch) = UnifiedDiff::new(
        &diff,
        &input,
        ConsumeBinaryHunk::new(String::new(), "\n"),
        ContextSize::symmetrical(3),
    )
    .consume() else {
        return html! {};
    };
    html! {
        @for line in patch.lines() {
            span class={ "ln " (diff_class(line)) } { (line) "\n" }
        }
    }
}

/// The CSS class for a unified-diff line, chosen from its leading marker
/// (mirrors `pre-redo:.../pages.rs`'s own `diff_class`).
fn diff_class(line: &str) -> &'static str {
    if line.starts_with("@@") {
        "hunk"
    } else if line.starts_with('+') {
        "add"
    } else if line.starts_with('-') {
        "del"
    } else {
        "ctx"
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "unit test")]

    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::empty("", false)]
    #[case::not_hex("zzzzzzz", false)]
    #[case::too_long(
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        false
    )]
    #[case::sha1("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", true)]
    fn parse_oid_accepts_only_well_formed_hex_ids(#[case] text: &str, #[case] valid: bool) {
        assert_eq!(parse_oid(text).is_ok(), valid);
    }

    #[test]
    fn diff_class_colors_hunk_add_del_and_context_lines() {
        assert_eq!(diff_class("@@ -1,2 +1,2 @@"), "hunk");
        assert_eq!(diff_class("+added"), "add");
        assert_eq!(diff_class("-removed"), "del");
        assert_eq!(diff_class(" context"), "ctx");
    }

    #[test]
    fn unified_diff_renders_colored_added_and_removed_lines() {
        let rendered = unified_diff("a\nb\n", "a\nc\n").into_string();
        assert!(rendered.contains("class=\"ln del\""));
        assert!(rendered.contains("class=\"ln add\""));
    }

    #[test]
    fn unified_diff_of_identical_text_renders_nothing() {
        let rendered = unified_diff("same\n", "same\n").into_string();
        assert!(rendered.is_empty());
    }
}
