//! `GET /reviews`: every review recorded in this repository, newest
//! first -- a read-only aggregate across commits, alongside
//! `crate::pages::commits`'s own per-commit `reviews_section` rather than
//! replacing it. Every mutation (starting a review, commenting on one)
//! still posts through `commits`'s own routes; this module has none of
//! its own.

use std::sync::Arc;

use gix::bstr::ByteSlice as _;
use gix_object::{Find, Write};
use maud::{Markup, html};

use crate::error::Result;
use crate::state::AppState;

/// `GET /reviews`: list [`ents_forge::review::list`]'s full result (no
/// `target` filter), newest reviewer-commit first. Best effort per row,
/// mirroring `crate::pages::commits::reviews_section`'s own stance: a
/// review whose reviewer-commit chain or target subject fails to read
/// still renders, just without the piece that failed, rather than
/// dropping the row or failing the page.
///
/// # Errors
///
/// Propagates a ref-store or object read failure enumerating the reviews
/// themselves ([`ents_forge::review::list`]); a per-row read failure
/// degrades that row instead of failing the page.
pub async fn list<O>(
    axum::extract::State(state): axum::extract::State<Arc<AppState<O>>>,
) -> Result<Markup>
where
    O: Find + Write + Send + 'static,
{
    let mut rows = ents_forge::review::list(state.refs.as_ref(), &*state.objects(), &state.path, None)
        .unwrap_or_default();
    let repo = gix::open(&state.path).ok();

    let mut with_time: Vec<(i64, Markup)> = rows
        .drain(..)
        .map(|((target, member), review)| {
            let reviewer = ents_model::namespace::review_ref(&target, &member)
                .ok()
                .and_then(|ref_name| state.refs.get(ref_name.as_ref()).ok().flatten())
                .and_then(|tip| super::commit_authorship(&*state.objects(), tip).ok());
            let seconds = reviewer.as_ref().map_or(0, |(_author, seconds)| *seconds);
            let subject = repo.as_ref().and_then(|repo| {
                let oid = gix_hash::ObjectId::from_hex(target.as_bytes()).ok()?;
                let commit = repo.find_commit(oid).ok()?;
                let message = commit.message().ok()?;
                Some(message.title.to_str_lossy().into_owned())
            });
            let short = target.get(..7).unwrap_or(&target).to_owned();
            (
                seconds,
                html! {
                    div.card {
                        div.comment-meta {
                            span class={ "verdict verdict-" (review.verdict) } { (review.verdict) }
                            (super::avatar(member.as_str()))
                            span.author { (member) }
                            span.spacer {}
                            a href={ "/commit/" (target) } { code { (short) } }
                            @if let Some(subject) = &subject {
                                span.muted { (subject) }
                            }
                        }
                        @if let Some((_author, seconds)) = &reviewer {
                            span.entry-size { (super::ago(*seconds)) }
                        }
                    }
                },
            )
        })
        .collect();
    with_time.sort_by(|(a, _), (b, _)| b.cmp(a));

    Ok(super::layout(
        &super::RepoHeader::from_state(&state),
        &super::identity_label(&state),
        super::Tab::Reviews,
        "Reviews",
        html! {
            div.readable {
                @if with_time.is_empty() {
                    (super::blankslate(
                        "No reviews yet",
                        html! { "Record one from a commit's own page." },
                    ))
                } @else {
                    @for (_seconds, card) in &with_time {
                        (card)
                    }
                }
            }
        },
    ))
}
