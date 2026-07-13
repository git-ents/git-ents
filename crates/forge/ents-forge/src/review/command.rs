//! The `review` command's business logic: review a commit (`model.review`),
//! writing both the review's own entity ref and its retention pin
//! (`model.review-pin`), list and read reviews back, and surface a
//! review's discussion thread by reusing [`crate::comment::thread`] rather
//! than duplicating context aggregation.
//!
//! Generalized over the same trait-object/generic seam
//! `crate::comment::command` uses (`&dyn RefStore`/`RefStoreRead`,
//! `impl Find`/`Find + Write`, `&dyn ents_receive::EventSink`) so a
//! composition root wires the concrete types and calls these functions,
//! never the other way around (`lens.parity`).

use ents_receive::{Identity, Mode, Outcome, propose_entity, propose_pin};
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

/// Read the [`Review`] at `id`'s ref tip, or [`Error::NotFound`] when no
/// such ref exists.
fn review_at(refs: &dyn RefStoreRead, objects: &impl Find, id: &str) -> Result<Review> {
    let ref_name = ents_model::namespace::review_ref(id)?;
    let Some(tip) = refs.get(ref_name.as_ref())? else {
        return Err(Error::NotFound {
            what: format!("review {id}"),
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
    /// The review's verdict (`approve`, `request-changes`, or any custom
    /// value — `model.extensibility`).
    pub verdict: String,
    /// The review's body text.
    pub body: String,
}

/// `git ents review new`: review `new.target`, writing both refs
/// `model.review` requires — the review's own entity ref at
/// `refs/meta/reviews/<id>`, and the retention pin at
/// `refs/meta/pins/reviews/<id>` keeping the reviewed commit (and its
/// ancestry) reachable (`model.review-pin`) — under one locally generated
/// id shared by both.
///
/// The two refs are written as two separate proposals to [`crate::comment::add`]'s
/// sibling primitives, [`propose_entity`] and [`propose_pin`]: `receive`
/// applies one `Proposal`'s transitions atomically, but a `Proposal` is not
/// itself parameterized to mix an entity-tree transition with an
/// empty-tree pin transition in one call, so this command reaches two
/// separate, sequential outcomes rather than one atomic batch — this is
/// the two-proposals shape the model accepts (`model.review`,
/// `model.review-pin`), not a gap to close.
///
/// # Errors
///
/// [`Error::InvalidArgument`] if `new.target` does not resolve to a commit;
/// otherwise propagates serialization or `receive` failures.
// @relation(model.review, model.review-pin, lens.parity, scope=function)
pub fn new(
    refs: &dyn RefStore,
    objects: &(impl Find + Write),
    events: &dyn ents_receive::EventSink,
    repo_path: &std::path::Path,
    new: NewReview,
    identity: &Identity<'_>,
    mode: Mode,
) -> Result<(String, Outcome, Outcome)> {
    let reviewed = resolve_commit(repo_path, &new.target)?;
    let review = Review::new(reviewed, new.verdict, new.body);

    // The review's id is derived locally, once, and shared by both refs
    // (`meta-ref.granularity`: one ref per review, one ref per pin) —
    // mirroring `crate::comment::command::add`'s own locally generated id.
    let id = uuid::Uuid::new_v4().simple().to_string();

    let entity_ref = ents_model::namespace::review_ref(&id)?;
    let entity_outcome = propose_entity(
        refs,
        objects,
        events,
        entity_ref,
        &review,
        identity,
        &format!("Review {reviewed}"),
        mode,
    )?;

    let pin_ref = ents_model::namespace::review_pin_ref(&id)?;
    let pin_outcome = propose_pin(
        refs,
        objects,
        events,
        pin_ref,
        reviewed,
        identity,
        &format!("Pin review {id}"),
        mode,
    )?;

    Ok((id, entity_outcome, pin_outcome))
}

/// `git ents review list [--target rev]`: every review recorded in this
/// repository, optionally filtered to those whose most recently reviewed
/// commit ([`Review::commit`]) resolves to `target`.
///
/// # Errors
///
/// [`Error::InvalidArgument`] if `target` is given but does not resolve;
/// otherwise propagates a ref-store or object read failure.
// @relation(model.review, scope=function)
pub fn list(
    refs: &dyn RefStoreRead,
    objects: &impl Find,
    repo_path: &std::path::Path,
    target: Option<&str>,
) -> Result<Vec<(String, Review)>> {
    let target_oid = target
        .map(|rev| resolve_commit(repo_path, rev))
        .transpose()?;
    let mut out = Vec::new();
    for entry in refs.iter_prefix("refs/meta/reviews/")? {
        let (name, tip) = entry?;
        let path = name.as_bstr().to_string();
        let Some(id) = path.strip_prefix("refs/meta/reviews/") else {
            continue;
        };
        let tree = commit_tree(objects, tip)?;
        let Ok(review) = facet_git_tree::deserialize::<Review>(&tree, objects) else {
            continue;
        };
        if let Some(target_oid) = target_oid
            && review.commit() != target_oid
        {
            continue;
        }
        out.push((id.to_owned(), review));
    }
    Ok(out)
}

/// `git ents review show`: `id`'s review, plus its discussion thread —
/// every [`Comment`] naming `reviews/<id>` as its context (or a reply into
/// one), reusing [`crate::comment::thread`] rather than a second
/// aggregation query (`model.comment-context`, `model.review`: "the review
/// itself MUST NOT store a list of its comments").
///
/// # Errors
///
/// [`Error::NotFound`] if `id` has no review ref; otherwise propagates a
/// ref-store or object read failure.
// @relation(model.review, model.comment-context, lens.parity, scope=function)
pub fn show(
    refs: &dyn RefStoreRead,
    objects: &impl Find,
    id: &str,
) -> Result<(Review, Vec<(String, Comment)>)> {
    let review = review_at(refs, objects, id)?;
    let context = format!("reviews/{id}");
    let thread = crate::comment::thread(refs, objects, &context)?;
    Ok((review, thread))
}
