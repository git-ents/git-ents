//! The `review` command's business logic: review a commit (`model.review`),
//! writing both the review's own entity ref and its retention pin
//! (`model.review-pin`), advancing the same composite-keyed ref on
//! re-review rather than minting a new one, list and read reviews back,
//! and surface a review's discussion thread by reusing
//! [`crate::comment::thread`] rather than duplicating context aggregation.
//!
//! Generalized over the same trait-object/generic seam
//! `crate::comment::command` uses (`&dyn RefStore`/`RefStoreRead`,
//! `impl Find`/`Find + Write`, `&dyn ents_receive::EventSink`) so a
//! composition root wires the concrete types and calls these functions,
//! never the other way around (`lens.parity`).

use ents_model::MemberId;
use ents_receive::{Identity, Mode, Outcome, propose_entity_with_pin};
use gix_hash::ObjectId;
use gix_object::{CommitRef, Find, Kind, Write};
use gix_ref_store::{RefStore, RefStoreRead};

use super::Review;
use crate::comment::Comment;
use crate::error::{Error, Result};

/// The tree of the commit at `oid` — duplicated from
/// `crate::comment::command`'s own copy of this helper (itself duplicated
/// from `ents_effect::run` and the CLI's `crate::commands::commit_tree`):
/// three ~15-line copies of "read a commit's tree oid via `Find`" is the
/// accepted pattern this codebase's own comment-command doc names, and
/// `review` and `comment` are sibling modules under this crate rather than
/// one importing the other's private helper, so this is a fourth.
fn commit_tree(objects: &impl Find, oid: ObjectId) -> Result<ObjectId> {
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

/// Resolve `rev` (a hex id, ref name, or revspec) to the commit it names in
/// the repository at `repo_path`.
fn resolve_commit(repo_path: &std::path::Path, rev: &str) -> Result<ObjectId> {
    let repo = gix::open(repo_path)?;
    resolve_in(&repo, rev)
}

/// [`resolve_commit`], against an already-open `repo` — [`new`] already
/// holds one open (to check re-review ancestry), so it resolves its target
/// through this rather than opening the repository a second time.
fn resolve_in(repo: &gix::Repository, rev: &str) -> Result<ObjectId> {
    let resolve = || Error::InvalidArgument(format!("cannot resolve {rev} to a commit"));
    let id = repo
        .rev_parse_single(rev)
        .map_err(|_source| resolve())?
        .object()
        .map_err(|_source| resolve())?
        .peel_to_kind(gix::object::Kind::Commit)
        .map_err(|_source| resolve())?
        .id;
    Ok(id)
}

/// Whether `ancestor` is `descendant` itself, or reachable from it by parent
/// edges — checked via the open repository's own merge-base machinery
/// rather than a bespoke walk. A `false` result on a lookup failure (no
/// shared history at all) is the correct answer, not an error to propagate:
/// it just means this is not the review [`new`] should advance.
fn is_ancestor_or_self(repo: &gix::Repository, ancestor: ObjectId, descendant: ObjectId) -> bool {
    ancestor == descendant
        || repo
            .merge_base(ancestor, descendant)
            .is_ok_and(|base| base.detach() == ancestor)
}

/// This member's existing review, if any, whose recorded target
/// ([`Review::target`]) is `reviewed` itself or one of its ancestors — the
/// fast-forward re-review case `model.review-pin` describes: "re-reviewing
/// after the target moves". Returns the found review's own genesis target
/// segment (the refname's `<target>`, parsed back via
/// [`ents_model::namespace::parse_review_ref`]) for [`new`] to advance the
/// same two refs under, rather than minting fresh ones.
fn find_review_to_advance(
    refs: &dyn RefStoreRead,
    objects: &impl Find,
    repo: &gix::Repository,
    member: &MemberId,
    reviewed: ObjectId,
) -> Result<Option<String>> {
    for entry in refs.iter_prefix("refs/meta/reviews/")? {
        let (name, tip) = entry?;
        let Some((target, entry_member)) = ents_model::namespace::parse_review_ref(name.as_ref())
        else {
            continue;
        };
        if entry_member != *member {
            continue;
        }
        let tree = commit_tree(objects, tip)?;
        let Ok(review) = facet_git_tree::deserialize::<Review>(&tree, objects) else {
            continue;
        };
        if is_ancestor_or_self(repo, review.target(), reviewed) {
            return Ok(Some(target));
        }
    }
    Ok(None)
}

/// Read the [`Review`] at `target`/`member`'s ref tip, or [`Error::NotFound`]
/// when no such ref exists.
fn review_at(
    refs: &dyn RefStoreRead,
    objects: &impl Find,
    target: &str,
    member: &MemberId,
) -> Result<Review> {
    let ref_name = ents_model::namespace::review_ref(target, member)?;
    let Some(tip) = refs.get(ref_name.as_ref())? else {
        return Err(Error::NotFound {
            what: format!("review {target}/{member}"),
        });
    };
    let tree = commit_tree(objects, tip)?;
    Ok(facet_git_tree::deserialize(&tree, objects)?)
}

/// What `git ents review new` writes: the revision to review, its verdict,
/// and its body.
#[derive(Debug, Clone)]
pub struct NewReview {
    /// The revision to review; resolved to a commit before writing.
    pub target: String,
    /// The review's verdict.
    pub verdict: super::Verdict,
    /// The review's body text.
    pub body: String,
}

/// `git ents review new`: review `new.target` as `member`, writing both
/// refs `model.review` requires — the review's own entity ref at
/// `refs/meta/reviews/<target>/<member>`, and the retention pin at
/// `refs/meta/pins/reviews/<target>/<member>` keeping the reviewed commit
/// (and its ancestry) reachable (`model.review-pin`) — a composite natural
/// key, no minted id anywhere (`meta-ref.identity-binding`).
///
/// When `member` already has a review whose own recorded target
/// ([`Review::target`]) is `new.target` itself or one of its ancestors,
/// this is a re-review: the *same* two refs advance fast-forward under the
/// original genesis target segment, with [`Review::target`] updated to the
/// newly reviewed commit, rather than a fresh pair being minted
/// (`model.review-pin`: "re-reviewing after the target moves MUST advance
/// the pin fast-forward"). Otherwise this is the review's genesis, keyed by
/// the reviewed commit's own oid.
///
/// The two refs travel in one atomic mutation via
/// [`propose_entity_with_pin`] (`receive.multi-ref-atomicity`): the
/// ref-store's atomic multi-ref compare-and-swap admits or refuses both
/// transitions together, so a review is never left with its entity written
/// but its retention pin missing. One [`Outcome`] covers the whole batch.
///
/// Returns the composite key's target segment (the review's genesis
/// target, unchanged across re-reviews) alongside the reached [`Outcome`].
///
/// # Errors
///
/// [`Error::InvalidArgument`] if `new.target` does not resolve to a commit;
/// otherwise propagates serialization or `receive` failures.
// @relation(model.review, model.review-pin, meta-ref.identity-binding, receive.multi-ref-atomicity, lens.parity, scope=function)
#[expect(
    clippy::too_many_arguments,
    reason = "one field per mutation shape (refs, objects, events, repo, the draft, the acting \
              member, identity, mode), mirroring propose_entity_with_pin's identically-justified \
              shape one layer down"
)]
pub fn new(
    refs: &dyn RefStore,
    objects: &(impl Find + Write),
    events: &dyn ents_receive::EventSink,
    repo_path: &std::path::Path,
    new: NewReview,
    member: &MemberId,
    identity: &Identity<'_>,
    mode: Mode,
) -> Result<(String, Outcome)> {
    let repo = gix::open(repo_path)?;
    let reviewed = resolve_in(&repo, &new.target)?;
    let review = Review::new(reviewed, new.verdict, new.body);

    // Re-reviewing after the target moves advances the SAME ref rather than
    // minting a new one (`model.review-pin`): find this member's existing
    // review, if any, whose own recorded target is an ancestor of (or equal
    // to) the commit reviewed now, and advance it in place.
    let target_hex = find_review_to_advance(refs, objects, &repo, member, reviewed)?
        .unwrap_or_else(|| reviewed.to_string());

    let outcome = propose_entity_with_pin(
        refs,
        objects,
        events,
        ents_model::namespace::review_ref(&target_hex, member)?,
        &review,
        ents_model::namespace::review_pin_ref(&target_hex, member)?,
        reviewed,
        identity,
        &format!("Review {reviewed}"),
        &format!("Pin review {target_hex}/{member}"),
        mode,
    )?;

    Ok((target_hex, outcome))
}

/// `git ents review list [--target rev]`: every review recorded in this
/// repository, keyed by its composite `(target, member)` segments
/// (`model.review`), optionally filtered to those whose most recently
/// reviewed commit ([`Review::target`]) resolves to `target`.
///
/// # Errors
///
/// [`Error::InvalidArgument`] if `target` is given but does not resolve;
/// otherwise propagates a ref-store or object read failure.
// @relation(model.review, meta-ref.identity-binding, scope=function)
pub fn list(
    refs: &dyn RefStoreRead,
    objects: &impl Find,
    repo_path: &std::path::Path,
    target: Option<&str>,
) -> Result<Vec<((String, MemberId), Review)>> {
    let target_oid = target
        .map(|rev| resolve_commit(repo_path, rev))
        .transpose()?;
    let mut out = Vec::new();
    for entry in refs.iter_prefix("refs/meta/reviews/")? {
        let (name, tip) = entry?;
        let Some((target_hex, member)) = ents_model::namespace::parse_review_ref(name.as_ref())
        else {
            continue;
        };
        let tree = commit_tree(objects, tip)?;
        let Ok(review) = facet_git_tree::deserialize::<Review>(&tree, objects) else {
            continue;
        };
        if let Some(target_oid) = target_oid
            && review.target() != target_oid
        {
            continue;
        }
        out.push(((target_hex, member), review));
    }
    Ok(out)
}

/// `git ents review withdraw`: retract `member`'s own review of `target`,
/// leaving the prior verdict in history rather than erasing it
/// (`model.review`). Resolves `target` (a revision) exactly as [`new`]
/// does, then reuses [`find_review_to_advance`] to locate `member`'s
/// *existing* review whose recorded target ([`Review::target`]) is
/// `target` itself or one of its ancestors — the same fast-forward lookup
/// `new` performs before a re-review, so a withdrawal reaches the review
/// even if it has since advanced past the commit named here. That review's
/// [`Review::withdrawn`] copy — same `target`, `verdict`, and `body`, only
/// `state` flipped — is written back onto the *same* two refs via
/// [`propose_entity_with_pin`], the identical advance/ref-writing path
/// `new` uses: no parallel write path exists for withdrawal
/// (`model.review-pin`, `receive.multi-ref-atomicity`).
///
/// Ownership is enforced entirely by `ents-gate`'s existing checks on the
/// `refs/meta/reviews/<target>/<member>` namespace — `identity_binding`'s
/// `Namespace::Review` arm (a review must be signed by the exact `member`
/// its own refname names) and `owner_mutation`'s `Namespace::Review` arm
/// (only that same signer may advance it) — so this function does not
/// re-check who `member` is; it only ever builds and writes
/// `reviews/<target>/<member>`, `member`'s own ref, and lets the gate
/// refuse anything else the same way it already refuses a mismatched
/// re-review (`gate.identity-binding`, `gate.owner-mutation`).
///
/// Withdrawing an already-withdrawn review is not an error: the found
/// review's `withdrawn()` copy of a `Withdrawn` review is itself
/// `Withdrawn`, so this simply re-writes the same state — a harmless
/// no-op-ish advance, not a special case this function detects.
///
/// # Errors
///
/// [`Error::InvalidArgument`] if `target` does not resolve to a commit;
/// [`Error::NotFound`] if `member` has no existing review reaching
/// `target` — there is nothing to withdraw; otherwise propagates
/// serialization or `receive` failures.
// @relation(model.review, model.review-pin, meta-ref.identity-binding, receive.multi-ref-atomicity, lens.parity, scope=function)
#[expect(
    clippy::too_many_arguments,
    reason = "one field per mutation shape, mirroring new's identically-justified shape"
)]
pub fn withdraw(
    refs: &dyn RefStore,
    objects: &(impl Find + Write),
    events: &dyn ents_receive::EventSink,
    repo_path: &std::path::Path,
    target: &str,
    member: &MemberId,
    identity: &Identity<'_>,
    mode: Mode,
) -> Result<(String, Outcome)> {
    let repo = gix::open(repo_path)?;
    let reviewed = resolve_in(&repo, target)?;

    let target_hex = find_review_to_advance(refs, objects, &repo, member, reviewed)?.ok_or_else(
        || Error::NotFound {
            what: format!("review of {reviewed} by {member}"),
        },
    )?;
    let existing = review_at(refs, objects, &target_hex, member)?;
    let withdrawn = existing.withdrawn();
    let retained = existing.target();

    let outcome = propose_entity_with_pin(
        refs,
        objects,
        events,
        ents_model::namespace::review_ref(&target_hex, member)?,
        &withdrawn,
        ents_model::namespace::review_pin_ref(&target_hex, member)?,
        retained,
        identity,
        &format!("Withdraw review {retained}"),
        &format!("Pin review {target_hex}/{member}"),
        mode,
    )?;

    Ok((target_hex, outcome))
}

/// `git ents review show`: `target`/`member`'s review, plus its discussion
/// thread — every [`Comment`] naming `reviews/<target>/<member>` as its
/// context (or a reply into one), reusing [`crate::comment::thread`] rather
/// than a second aggregation query (`model.comment-context`, `model.review`:
/// "the review itself MUST NOT store a list of its comments").
///
/// # Errors
///
/// [`Error::NotFound`] if `target`/`member` has no review ref; otherwise
/// propagates a ref-store or object read failure.
// @relation(model.review, model.comment-context, lens.parity, scope=function)
pub fn show(
    refs: &dyn RefStoreRead,
    objects: &impl Find,
    target: &str,
    member: &MemberId,
) -> Result<(Review, Vec<(String, Comment)>)> {
    let review = review_at(refs, objects, target, member)?;
    let context = format!("reviews/{target}/{member}");
    let thread = crate::comment::thread(refs, objects, &context)?;
    Ok((review, thread))
}
