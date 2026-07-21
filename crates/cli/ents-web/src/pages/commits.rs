//! `GET /commits`, `GET /commit/{oid}`: a read-only commit history and
//! per-commit unified diff over `HEAD` -- a tab of its own (both routes
//! render with `super::Tab::Commits` active; see [`super`]'s own doc),
//! also reached from [`super::files`]'s "history" link.
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
//! `diff_class` then colorizes line by line -- no new dependency, since
//! `gix`'s default features already enable `blob-diff`.
//!
//! `GET /commit/{oid}` also lists a "conversation": every comment whose
//! anchor was captured against that exact commit
//! (`crate::pages::comments::for_commit`), rendered below the diff via the
//! same `crate::pages::comments::comment_card` a blob view uses, each
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

/// One row of `GET /commits` -- also what [`super::dashboard`]'s History
/// card renders, at its own smaller limit, so the two pages share one
/// history read.
pub(crate) struct CommitRow {
    /// The full commit id, the `/commit/{oid}` link target.
    pub(crate) oid: ObjectId,
    /// [`super::short_oid`] of `oid`, the row's displayed, mono id.
    pub(crate) short: String,
    /// The commit message's title line.
    pub(crate) subject: String,
    /// The commit author's display name.
    pub(crate) author: String,
    /// [`super::ago`] of the commit author's time.
    pub(crate) ago: String,
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
    let (rows, older) = commit_rows(&state, params.from.as_deref(), PAGE_SIZE);
    Ok(super::layout(
        &super::RepoHeader::from_state(&state),
        &super::identity_label(&state),
        super::Tab::Commits,
        "Commits",
        html! {
            @if rows.is_empty() {
                (blankslate())
            } @else {
                div.card.history {
                    @for row in &rows {
                        (commit_row(row))
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

/// Up to `limit` rows starting at `from` (or `HEAD` when `from` is
/// `None`), newest first, plus the oid to continue from for an "older"
/// link when more commits remain -- [`list`] passes [`PAGE_SIZE`],
/// [`super::dashboard`]'s History card its own smaller cap. Best-effort:
/// an unopenable repository, an unborn `HEAD`, or an
/// unparsable/unresolvable `from` all degrade to an empty page rather
/// than an error.
pub(crate) fn commit_rows<O>(
    state: &AppState<O>,
    from: Option<&str>,
    limit: usize,
) -> (Vec<CommitRow>, Option<String>) {
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
        if rows.len() == limit {
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

/// One [`CommitRow`] as a `.card-row` (the design's `CommitRow` component,
/// README's Commits/Commit-detail screens): its short oid as an accent mono
/// link, a `.scope` chip ([`super::split_scope`]/[`super::scope_class`])
/// when the subject carries a Scoped-Commits prefix, the (possibly
/// stripped) description ellipsized in the remaining space, the author,
/// and its relative age -- the one place a commit row's markup is spelled,
/// so [`list`]'s pager reads the same row [`super::dashboard`]'s History
/// card already renders.
fn commit_row(row: &CommitRow) -> Markup {
    html! {
        div.card-row {
            a href={ "/commit/" (row.oid) } { code { (row.short) } }
            @match super::split_scope(&row.subject) {
                Some((scope, rest)) => {
                    span class={ "scope " (super::scope_class(scope)) } { (scope) }
                    span.desk-subject { (rest) }
                },
                None => { span.desk-subject { (row.subject) } },
            }
            span.row-author { (row.author) }
            span.row-when { (row.ago) }
        }
    }
}

/// The empty-history placeholder ([`super::blankslate`]): an unborn
/// `HEAD`, or a repository this page could not open at all.
fn blankslate() -> Markup {
    super::blankslate(
        "No commits yet",
        html! { "This repository has no history to show." },
    )
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
    let (diff, truncated) = diff_sections(&state, &repo, old_tree_ref, &new_tree);
    let comments = super::comments::for_commit(&state, object_id);
    let checks = checks_section(&state, object_id);
    let reviews = reviews_section(&state, &session, object_id, &oid);
    let (sidebar_rows, _older) = commit_rows(&state, None, PAGE_SIZE);

    Ok(super::layout_split(
        &super::RepoHeader::from_state(&state),
        &super::identity_label(&state),
        super::Tab::Commits,
        &subject,
        false,
        commits_sidebar(&sidebar_rows, object_id),
        html! {
            (super::child_crumbs("commits", "/commits", &super::short_oid(&object_id)))
            // The commit card, its reviews, and the conversation are
            // single-column reading content, capped at `.readable`'s
            // narrow width; only the diff sections between them keep the
            // shell's full width (see `ents.css`'s own `.readable` note).
            div.readable {
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
                (checks)
                (reviews)
            }
            (diff)
            @if truncated {
                div.card { div.binary { "Diff truncated (over " (MAX_DIFF_BYTES / (1024 * 1024)) " MiB)." } }
            }
            @if !comments.is_empty() {
                div.readable {
                    h2 { "Conversation" }
                    @for (index, comment) in comments.iter().enumerate() {
                        (super::comments::comment_card(index, comment, super::comments::LinkMode::CrossFile))
                    }
                }
            }
        },
    ))
}

/// The Review split's `.tree` sidebar (`crate::pages::layout_split`): the
/// most recent commits, the viewed one active, each row its short oid and
/// subject on one ellipsized line, closed by a link into the full pager.
/// A commit older than the newest [`PAGE_SIZE`] simply highlights nothing
/// -- the sidebar is a recency lane, not a second pager.
fn commits_sidebar(rows: &[CommitRow], current: ObjectId) -> Markup {
    html! {
        @if rows.is_empty() {
            span.tree-note { "No history to show." }
        }
        @for row in rows {
            a.active[row.oid == current] href={ "/commit/" (row.oid) } {
                (row.short) " " (row.subject)
            }
        }
        a href="/commits" { "all commits \u{2192}" }
    }
}

/// One row of the commit page's "Checks" card: a recorded result targeting
/// the shown commit.
struct CheckRow {
    /// The recording effect's name ([`ents_model::ResultRecord`]'s own
    /// `effect` field), the row's `/effects/{name}` link.
    effect: String,
    /// The run's outcome, one of the closed taxonomy's three values.
    status: ents_model::Status,
    /// The self-run mirror's `<member>` segment when the result lives
    /// there rather than the canonical namespace (`effect.self-run`).
    self_run: Option<String>,
    /// The result ref tip's author time, for [`super::ago`].
    seconds: Option<i64>,
}

/// A [`ents_model::Status`]'s display word, doubling as its
/// `.status-<word>` chip class -- the closed pass/fail/error taxonomy
/// (`model.result-taxonomy`), spelled out here rather than through
/// `Debug`.
fn status_label(status: ents_model::Status) -> &'static str {
    match status {
        ents_model::Status::Pass => "pass",
        ents_model::Status::Fail => "fail",
        ents_model::Status::Error => "error",
    }
}

/// The "Checks" card on `GET /commit/{oid}`: every recorded result
/// (`model.result-identity`) whose stored `target` field names this
/// commit -- the canonical `refs/meta/results/<effect>/<short-oid>`
/// namespace and every member's self-run mirror
/// (`refs/meta/self/<member>/...`), matched on the tree's own `target`
/// field (the same binding the gate verifies), never the refname's
/// short-oid segment. Renders nothing at all when no result targets the
/// commit: a result is only ever written by a run
/// (`effect.result-taxonomy`), so "no checks" is the ordinary state of
/// most commits, not a pending one. Best effort: a result ref whose tree
/// cannot be read back is skipped from this card (it still lists on
/// `git ents effect log`).
// @relation(model.result-identity, model.result-taxonomy, scope=function)
fn checks_section<O: Find + Write>(state: &AppState<O>, commit_id: ObjectId) -> Markup {
    let mut rows: Vec<CheckRow> = Vec::new();
    for prefix in ["refs/meta/results/", "refs/meta/self/"] {
        let Ok(iter) = state.refs.iter_prefix(prefix) else {
            continue;
        };
        for entry in iter {
            let Ok((name, tip)) = entry else { continue };
            // One `state.objects()` lock per read -- the same
            // non-reentrant-`Mutex` care `crate::pages::effects::read_all`
            // documents.
            let record = {
                let objects = state.objects();
                super::commit_tree(&*objects, tip).ok().and_then(|tree| {
                    facet_git_tree::deserialize::<ents_model::ResultRecord>(&tree, &*objects).ok()
                })
            };
            let Some(record) = record else { continue };
            if record.target() != commit_id {
                continue;
            }
            let path = name.as_bstr().to_string();
            let self_run = path
                .strip_prefix("refs/meta/self/")
                .and_then(|rest| rest.split('/').next())
                .map(str::to_owned);
            let seconds = super::commit_authorship(&*state.objects(), tip)
                .ok()
                .map(|(_author, seconds)| seconds);
            rows.push(CheckRow {
                effect: record.effect,
                status: record.status,
                self_run,
                seconds,
            });
        }
    }
    if rows.is_empty() {
        return html! {};
    }
    rows.sort_by(|a, b| (&a.effect, &a.self_run).cmp(&(&b.effect, &b.self_run)));
    html! {
        div.card {
            div.card-header { "Checks" }
            @for row in &rows {
                div.card-row {
                    span class={ "status status-" (status_label(row.status)) } {
                        (status_label(row.status))
                    }
                    " "
                    a href={ "/effects/" (row.effect) } { (row.effect) }
                    @if let Some(member) = &row.self_run {
                        span.muted { " \u{b7} self-run by " (member) }
                    }
                    @if let Some(seconds) = row.seconds {
                        span.entry-size { (super::ago(seconds)) }
                    }
                }
            }
        }
    }
}

/// Every review targeting `commit_id` (`ents_forge::review::list` filtered
/// to this commit, `model.review`), each rendering its verdict prominently,
/// its body as AsciiDoc, and its reviewer (from the review ref's own tip
/// commit chain, `meta-ref.identity-binding` -- a review stores no author
/// field, only its composite `(target, member)` key), followed by its
/// discussion: the comments naming `reviews/<target>/<member>` as their
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
        h2 { "Reviews" }
        @if reviews.is_empty() {
            p.muted { "No reviews of this commit yet \u{2014} record a verdict below." }
        }
        @for ((target, member), review) in &reviews {
            div.card {
                div.comment-meta {
                    span class={ "verdict verdict-" (review.verdict) } { (review.verdict) }
                    (super::avatar(member.as_str()))
                    span.author { (member) }
                    @let reviewer = ents_model::namespace::review_ref(target, member)
                        .ok()
                        .and_then(|ref_name| state.refs.get(ref_name.as_ref()).ok().flatten())
                        .and_then(|tip| super::commit_authorship(&*state.objects(), tip).ok());
                    @if let Some((_author, seconds)) = &reviewer {
                        span { (super::ago(*seconds)) }
                    }
                }
                div.doc-body {
                    (crate::asciidoc::to_html(&review.body).unwrap_or_else(|_| html! { p { (review.body) } }))
                }
                @let thread = ents_forge::comment::thread(
                    state.refs.as_ref(),
                    &*state.objects(),
                    &format!("reviews/{target}/{member}"),
                ).unwrap_or_default();
                (super::comments::thread_section(state, session, &thread, &return_to))
                (review_comment_form(session, target, member, &return_to))
            }
        }
        (start_review_form(session, oid))
    }
}

/// The comment-on-this-review form (`POST /reviews/{target}/{member}/comment`):
/// a contextual comment naming `reviews/<target>/<member>`
/// (`model.comment-context`), so a review's discussion can start from the
/// web and not only the CLI or lens.
fn review_comment_form(
    session: &Session,
    target: &str,
    member: &ents_model::MemberId,
    return_to: &str,
) -> Markup {
    html! {
        form method="post" action=(format!("/reviews/{target}/{member}/comment")) {
            (super::csrf_input(session))
            input type="hidden" name="return_to" value=(return_to);
            label { "Comment on this review" textarea name="body" {} }
            button type="submit" { "Comment" }
        }
    }
}

/// The form fields `POST /reviews/{target}/{member}/comment` accepts.
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

/// `POST /reviews/{target}/{member}/comment`: a comment naming
/// `reviews/<target>/<member>` as its context (`model.comment-context`) --
/// an ordinary [`ents_forge::comment::add`], contextual and unanchored,
/// joining the review's discussion thread the moment it lands.
///
/// # Errors
///
/// [`Error::BadCsrf`] if `form.csrf` does not match; otherwise propagates
/// [`ents_forge::comment::add`]'s own failures.
// @relation(model.comment-context, roots.web-signing, roots.web-session, scope=function)
pub async fn review_comment<O>(
    State(state): State<Arc<AppState<O>>>,
    axum::Extension(session): axum::Extension<Session>,
    Path((target, member)): Path<(String, String)>,
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
        context: Some(format!("reviews/{target}/{member}")),
        parent: None,
    };
    let (_comment_id, outcome) = ents_forge::comment::add(
        state.refs.as_ref(),
        &*state.objects(),
        state.events.as_ref(),
        &state.path,
        new,
        &crate::receive_identity!(identity, crate::pages::member_author(&session)),
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

/// The start-a-review form (`POST /commit/{oid}/review`): a verdict and
/// a body. The verdict is a closed `.picker` (README's `VerdictPicker`) of
/// radio inputs over [`ents_forge::review::Verdict`]'s three variants --
/// `model.review` makes it a hard enum, unlike issue and comment states --
/// defaulting to `approve`, the same default a bare `select`'s first option
/// would submit.
fn start_review_form(session: &Session, oid: &str) -> Markup {
    html! {
        h3 { "Start a review" }
        form method="post" action=(format!("/commit/{oid}/review")) {
            (super::csrf_input(session))
            p.muted { "verdict" }
            div.picker {
                label.opt {
                    input type="radio" name="verdict" value="approve" checked;
                    span.dot {}
                    "approve"
                }
                label.opt {
                    input type="radio" name="verdict" value="request-changes";
                    span.dot {}
                    "request-changes"
                }
                label.opt {
                    input type="radio" name="verdict" value="comment";
                    span.dot {}
                    "comment"
                }
            }
            label { "Body" textarea name="body" {} }
            button type="submit" { "Start a Review" }
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
    let member = reviewer_member_id(&state);
    let identity = state.identity.as_ref();
    let new = ents_forge::review::NewReview {
        target: oid.clone(),
        verdict: form.verdict.parse().map_err(|_unknown| {
            Error::InvalidArgument(format!("unknown verdict: {}", form.verdict))
        })?,
        body: form.body,
    };
    let (_target, outcome) = ents_forge::review::new(
        state.refs.as_ref(),
        &*state.objects(),
        state.events.as_ref(),
        &state.path,
        new,
        &member,
        &crate::receive_identity!(identity, crate::pages::member_author(&session)),
        state.mode,
    )?;
    crate::error::outcome_to_result(outcome)?;
    Ok(Redirect::to(&format!("/commit/{oid}")))
}

/// The acting session's member id -- the composite review key's
/// `<member>` segment -- resolved the same way
/// [`super::account::resolve_member_by_key`] does, falling back to a short
/// hash of the public key when no enrolled member matches: mirrors
/// `git_ents::commands::serve::build_state`'s identical fallback
/// (`roots.web-signing`: an unenrolled local identity may still review,
/// exactly as it may still browse and comment).
fn reviewer_member_id<O: Find>(state: &AppState<O>) -> ents_model::MemberId {
    let pubkey = state.identity.public_openssh();
    super::account::resolve_member_by_key(state, &pubkey)
        .map(|(id, _member)| id)
        .unwrap_or_else(|_source| ents_model::MemberId::new(short_key_fingerprint(&pubkey)))
}

/// The first twelve characters of `pubkey`'s key-material token -- mirrors
/// `git_ents::commands::short_fingerprint`'s identical fallback label.
fn short_key_fingerprint(pubkey: &str) -> String {
    let hex: String = pubkey
        .split_whitespace()
        .nth(1)
        .unwrap_or(pubkey)
        .chars()
        .take(12)
        .collect();
    if hex.is_empty() {
        "member".to_owned()
    } else {
        hex
    }
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
fn diff_sections<O>(
    state: &AppState<O>,
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
        // The walk yields every intermediate directory as its own tree
        // change; only blob (and link) entries are files a reader can
        // diff, so a tree entry renders nothing rather than a bare
        // header per subdirectory.
        if is_tree_change(&change) {
            return Ok(std::ops::ControlFlow::Continue(()));
        }
        let (section, bytes) = render_change(state, repo, &change);
        total = total.saturating_add(bytes);
        sections.push(section);
        if total > MAX_DIFF_BYTES {
            truncated = true;
        }
        Ok(std::ops::ControlFlow::Continue(()))
    });
    (html! { @for section in &sections { (section) } }, truncated)
}

/// Whether `change` is a tree (directory) entry rather than a blob or
/// link -- [`diff_sections`] skips these, since the walk names every
/// intermediate directory on the way to a changed file.
fn is_tree_change(change: &Change<'_, '_, '_>) -> bool {
    match *change {
        Change::Addition { entry_mode, .. }
        | Change::Deletion { entry_mode, .. }
        | Change::Modification { entry_mode, .. }
        | Change::Rewrite { entry_mode, .. } => entry_mode.is_tree(),
    }
}

/// One changed file's `.diff` section: a `.file`-classed header naming the
/// path (and, on a rename, the old path it moved from) beside its own
/// [`super::editor_open`] pill -- the web↔editor handoff motif beside every
/// code location (README's `EditorPill` inventory entry names diff headers
/// explicitly) -- followed by either a `.meta`-classed "binary file
/// changed" notice or the colorized unified diff between its old and new
/// blob content. Returns the section's rendered byte cost, so
/// [`diff_sections`] can track the page's overall budget.
fn render_change<O>(
    state: &AppState<O>,
    repo: &gix::Repository,
    change: &Change<'_, '_, '_>,
) -> (Markup, usize) {
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
            " "
            (super::editor_open(state, &path, None))
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
