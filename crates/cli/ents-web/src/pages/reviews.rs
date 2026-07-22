//! `GET /reviews`, `GET /reviews/{target}/{member}`,
//! `POST /reviews/{target}/{member}/withdraw`: the review surface's own
//! aggregate list and per-review detail page -- a read-only aggregate
//! across commits, alongside `crate::pages::commits`'s own per-commit
//! `reviews_section` rather than replacing it. Starting a review still only
//! ever posts through `commits`'s own route (`POST /commit/{oid}/review`);
//! commenting on one (`POST /reviews/{target}/{member}/comment`) is
//! `commits::review_comment`, shared verbatim by both pages that render a
//! review's thread. Withdrawing one is this module's own mutation: every
//! read is `ents_forge::review::{list,show}` and the withdraw write is
//! `ents_forge::review::withdraw` -- the web is another caller of that one
//! library func, never a second review-state machine (`lens.parity`).
//!
//! A review's own page ([`show`]) renders even when the review is
//! [`ents_forge::review::ReviewState::Withdrawn`] -- a direct link stays
//! live -- while [`list`] and [`reviews_sidebar`] both filter withdrawn
//! rows out, mirroring `commits::reviews_section`'s own stance: withdrawal
//! is append-only (`model.review`), it retracts a verdict from the
//! aggregate views, never from history or from the one page a direct link
//! still reaches.

use std::sync::Arc;

use axum::Form;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Redirect};
use ents_forge::review::{self, Review, ReviewState};
use ents_model::MemberId;
use gix::bstr::ByteSlice as _;
use gix_object::{Find, Write};
use maud::{Markup, html};
use serde::Deserialize;

use crate::error::Result;
use crate::session::Session;
use crate::state::AppState;

/// Every non-withdrawn review recorded in this repository
/// (`ents_forge::review::list`, no `target` filter), each paired with the
/// review ref's own tip-commit time when it could be read (`model.review`
/// stores no timestamp field of its own), newest first. The one aggregate
/// read [`list`]'s cards and [`reviews_sidebar`]'s rows both build from, so
/// the withdrawn filter and the ordering are computed in exactly one place.
/// Best effort throughout, mirroring `commits::reviews_section`'s own
/// stance: a review whose reviewer-commit chain fails to read still sorts
/// (last, its time treated as `0`) rather than dropping the row.
fn active_reviews<O: Find + Write>(
    state: &AppState<O>,
) -> Vec<(String, MemberId, Review, Option<i64>)> {
    let mut rows = review::list(state.refs.as_ref(), &*state.objects(), &state.path, None)
        .unwrap_or_default();
    rows.retain(|(_, review)| review.state != ReviewState::Withdrawn);
    let mut with_time: Vec<(String, MemberId, Review, Option<i64>)> = rows
        .into_iter()
        .map(|((target, member), review)| {
            let seconds = ents_model::namespace::review_ref(&target, &member)
                .ok()
                .and_then(|ref_name| state.refs.get(ref_name.as_ref()).ok().flatten())
                .and_then(|tip| super::commit_authorship(&*state.objects(), tip).ok())
                .map(|(_author, seconds)| seconds);
            (target, member, review, seconds)
        })
        .collect();
    with_time.sort_by_key(|(.., seconds)| std::cmp::Reverse(seconds.unwrap_or(0)));
    with_time
}

/// `GET /reviews`: [`active_reviews`]'s full result rendered as one card
/// per review -- verdict, reviewer, and the target commit's own subject --
/// beside [`reviews_sidebar`]'s compact newest-first nav (`crate::pages::layout_split`).
///
/// # Errors
///
/// Propagates a ref-store or object read failure enumerating the reviews
/// themselves ([`ents_forge::review::list`]); a per-row read failure
/// degrades that row instead of failing the page.
pub async fn list<O>(State(state): State<Arc<AppState<O>>>) -> Result<Markup>
where
    O: Find + Write + Send + 'static,
{
    let rows = active_reviews(&state);
    let repo = gix::open(&state.path).ok();

    let cards: Vec<Markup> = rows
        .iter()
        .map(|(target, member, review, seconds)| {
            let subject = repo.as_ref().and_then(|repo| {
                let oid = gix_hash::ObjectId::from_hex(target.as_bytes()).ok()?;
                let commit = repo.find_commit(oid).ok()?;
                let message = commit.message().ok()?;
                Some(message.title.to_str_lossy().into_owned())
            });
            let short = target.get(..7).unwrap_or(target).to_owned();
            html! {
                div.card {
                    div.comment-meta {
                        (super::verdict_chip(review.verdict))
                        (super::avatar(member.as_str()))
                        span.author { (member) }
                        span.spacer {}
                        a href={ "/reviews/" (target) "/" (member) } { code { (short) } }
                        @if let Some(subject) = &subject {
                            span.muted { (subject) }
                        }
                    }
                    @if let Some(seconds) = seconds {
                        span.entry-size { (super::ago(*seconds)) }
                    }
                }
            }
        })
        .collect();

    Ok(super::layout_split(
        &super::RepoHeader::from_state(&state),
        &super::identity_label(&state),
        super::Tab::Reviews,
        "Reviews",
        false,
        reviews_sidebar(&rows, None),
        html! {
            div.readable {
                @if cards.is_empty() {
                    (super::blankslate(
                        "No reviews yet",
                        html! { "Record one from a commit's own page." },
                    ))
                } @else {
                    @for card in &cards {
                        (card)
                    }
                }
            }
        },
    ))
}

/// The Reviews split's `.tree` sidebar (mirrors `issues::issues_sidebar`):
/// every [`active_reviews`] row as a two-line `.side-row` -- its verdict and
/// reviewer on the title line, the target commit's abbreviated id on the
/// locator line -- linking to [`show`]'s own page, `.active` naming the
/// viewed `(target, member)` pair. Withdrawn reviews are already filtered
/// out of `rows` by [`active_reviews`]; they stay reachable only by a
/// direct link to [`show`], never from this nav.
fn reviews_sidebar(
    rows: &[(String, MemberId, Review, Option<i64>)],
    active: Option<(&str, &str)>,
) -> Markup {
    html! {
        div.tree-head {
            span { "Reviews" }
        }
        @if rows.is_empty() {
            span.tree-note { "No reviews yet." }
        }
        @for (target, member, review, _seconds) in rows {
            a.side-row.active[active == Some((target.as_str(), member.as_str()))]
                href={ "/reviews/" (target) "/" (member) }
            {
                span.side-title {
                    (super::verdict_chip(review.verdict))
                    " " (member.as_str())
                }
                span.side-meta {
                    span.locator { "on " (ents_forge::abbreviate_id(target)) }
                }
            }
        }
    }
}

/// `GET /reviews/{target}/{member}`: one review
/// (`ents_forge::review::show`), its verdict/state/target/reviewer metadata
/// card, its body rendered as AsciiDoc, its discussion thread, a comment
/// composer, and -- only for the review's own author while it is still
/// [`ReviewState::Active`] -- a withdraw control. Renders even for a
/// withdrawn review (this module's own doc: a direct link stays live; only
/// [`list`]/[`reviews_sidebar`] hide a withdrawn row).
///
/// # Errors
///
/// [`crate::Error::Forge`] (wrapping [`ents_forge::Error::NotFound`]) if
/// `target`/`member` has no review ref at all; otherwise propagates a
/// ref-store or object read failure.
// @relation(model.review, model.comment-context, lens.parity, scope=function)
pub async fn show<O>(
    State(state): State<Arc<AppState<O>>>,
    axum::Extension(session): axum::Extension<Session>,
    Path((target, member)): Path<(String, String)>,
) -> Result<Markup>
where
    O: Find + Write + Send + 'static,
{
    let member = MemberId::new(member);
    let (review, thread) =
        review::show(state.refs.as_ref(), &*state.objects(), &target, &member)?;
    let reviewer = ents_model::namespace::review_ref(&target, &member)
        .ok()
        .and_then(|ref_name| state.refs.get(ref_name.as_ref()).ok().flatten())
        .and_then(|tip| super::commit_authorship(&*state.objects(), tip).ok());
    let body =
        crate::asciidoc::to_html(&review.body).unwrap_or_else(|_| html! { p { (review.body) } });
    let return_to = format!("/reviews/{target}/{member}");
    let is_author = super::reviewer_member_id(&state) == member;
    // Best-effort: the sidebar listing every other review beside this one
    // is navigation chrome, never a reason to fail this review's own page.
    let rows = active_reviews(&state);

    Ok(super::layout_split(
        &super::RepoHeader::from_state(&state),
        &super::identity_label(&state),
        super::Tab::Reviews,
        &format!("Review of {}", ents_forge::abbreviate_id(&target)),
        false,
        reviews_sidebar(&rows, Some((&target, member.as_str()))),
        html! {
            (super::child_crumbs("reviews", "/reviews", ents_forge::abbreviate_id(&target)))
            div.readable {
                div.card {
                    dl.entity-view {
                        dt { "verdict" }
                        dd { (super::verdict_chip(review.verdict)) }
                        dt { "state" }
                        dd { (state_badge(review.state)) }
                        dt { "target" }
                        dd { a href={ "/commit/" (target) } { code { (target) } } }
                        dt { "reviewer" }
                        dd { (super::avatar(member.as_str())) " @" (member.as_str()) }
                        dt { "reviewed" }
                        dd {
                            @if let Some((_author, seconds)) = &reviewer {
                                (super::ago(*seconds))
                            } @else {
                                span.muted { "unknown" }
                            }
                        }
                    }
                    div.doc-body { (body) }
                }
                @if review.state == ReviewState::Active {
                    @if is_author {
                        (withdraw_form(&session, &target, &member))
                    }
                } @else {
                    p.muted { "This review has been withdrawn." }
                }
                h2 { "Discussion" }
                @if thread.is_empty() {
                    (super::blankslate(
                        "No comments yet",
                        html! { "Start the discussion below." },
                    ))
                } @else {
                    (crate::pages::comments::thread_section(&state, &session, &thread, &return_to))
                }
                div.card {
                    div.card-header { "Add a comment" }
                    (super::commits::review_comment_form(&session, &target, &member, &return_to))
                }
            }
        },
    ))
}

/// The review detail card's `state` `dd` (see [`show`]): a plain neutral
/// `.chip.chip-pill` naming `active`, or the same grey `.state-closed`
/// treatment `issues::state_chip` gives a closed issue naming `withdrawn`
/// instead -- so a direct link to a retracted verdict states plainly, at a
/// glance, that it no longer stands.
fn state_badge(state: ReviewState) -> Markup {
    match state {
        ReviewState::Active => html! {
            span.chip.chip-pill { "active" }
        },
        ReviewState::Withdrawn => html! {
            span.chip.chip-pill.state-closed { "withdrawn" }
        },
    }
}

/// The withdraw-this-review control (`POST /reviews/{target}/{member}/withdraw`),
/// rendered by [`show`] only for the review's own author while it is still
/// [`ReviewState::Active`] -- retracting a verdict stays a decision only
/// its author can make, the same way `ents-gate`'s own `owner_mutation`
/// check refuses anyone else's attempt at the ref level
/// (`ents_forge::review::withdraw`'s own doc).
fn withdraw_form(session: &Session, target: &str, member: &MemberId) -> Markup {
    html! {
        form method="post" action=(format!("/reviews/{target}/{member}/withdraw")) {
            (super::csrf_input(session))
            button type="submit" { "Withdraw review" }
        }
    }
}

/// The form fields `POST /reviews/{target}/{member}/withdraw` accepts.
#[derive(Debug, Deserialize)]
pub struct WithdrawForm {
    /// The per-session CSRF token (`roots.web-session`).
    csrf: String,
}

/// `POST /reviews/{target}/{member}/withdraw`: retract the signed-in
/// member's own review of `target` (`ents_forge::review::withdraw`) -- the
/// web is another caller of that one library func, driving the identical
/// mutation `git ents review withdraw` does. The path's own `member`
/// segment names whose review [`show`] rendered, but the write always
/// targets the *signed-in* identity's own member id
/// ([`super::reviewer_member_id`]), never the path's: this handler can only
/// ever build and write `reviews/<target>/<the signer>`. A member who is
/// not the review's author therefore has no matching
/// `refs/meta/reviews/<target>/<member>` of their own to advance, and the
/// mutation fails with [`ents_forge::Error::NotFound`] rather than
/// touching anyone else's ref -- no divergent ownership check is added
/// here; `ents-gate`'s own `identity_binding`/`owner_mutation` checks on
/// this namespace back the same refusal up independently (see
/// [`ents_forge::review::withdraw`]'s own doc).
///
/// # Errors
///
/// [`crate::Error::BadCsrf`] if `form.csrf` does not match; otherwise
/// propagates [`ents_forge::review::withdraw`]'s own failures (including
/// [`ents_forge::Error::NotFound`] when the signed-in member has no review
/// reaching `target`).
// @relation(model.review, roots.web-signing, roots.web-session, lens.parity, scope=function)
pub async fn withdraw<O>(
    State(state): State<Arc<AppState<O>>>,
    axum::Extension(session): axum::Extension<Session>,
    Path((target, _member)): Path<(String, String)>,
    Form(form): Form<WithdrawForm>,
) -> Result<impl IntoResponse>
where
    O: Find + Write + Send + 'static,
{
    super::require_csrf(&session, &form.csrf)?;
    let member = super::reviewer_member_id(&state);
    let identity = state.identity.as_ref();
    let (target_hex, outcome) = review::withdraw(
        state.refs.as_ref(),
        &*state.objects(),
        state.events.as_ref(),
        &state.path,
        &target,
        &member,
        &crate::receive_identity!(identity, crate::pages::member_author(&session)),
        state.mode,
    )?;
    crate::error::outcome_to_result(outcome)?;
    Ok(Redirect::to(&format!("/reviews/{target_hex}/{member}")))
}
