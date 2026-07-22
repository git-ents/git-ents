//! `git ents review`: a thin wrapper around `ents_forge::review`'s
//! business logic — this module only resolves the signer/actor identity
//! against [`LocalRoot`] (plus, for [`new`], the reviewer's own member id —
//! the composite key's `<member>` segment, `meta-ref.identity-binding`) and
//! translates a reached `Outcome` into a CLI-facing [`Result`]
//! (`crate::mutate::outcome_to_result`), exactly as every other mutation
//! command does. Every operation is the library call itself
//! (`lens.parity`); nothing here re-implements one.

use ents_forge::comment::Comment;
use ents_forge::review;
use ents_forge::review::{NewReview, Review};
use ents_model::MemberId;
use ents_receive::Identity;

use super::{actor, signer};
use crate::error::Result;
use crate::mutate::outcome_to_result;
use crate::root::LocalRoot;

/// `git ents review new`: review a commit as the signer's own member,
/// writing both its entity ref and its retention pin — or, when this
/// member already has a review of an ancestor of the target, advancing
/// that same review fast-forward (`model.review-pin`).
///
/// # Errors
///
/// [`crate::error::Error::Forge`] if `new.target` does not resolve to a
/// commit, or serialization or `receive` itself fails for either ref; see
/// [`crate::mutate::outcome_to_result`] for how a reached refusal renders.
pub fn new(root: &LocalRoot, new: NewReview, key: Option<std::path::PathBuf>) -> Result<String> {
    let signer = signer(root, key)?;
    let member = reviewer_member_id(root, &signer)?;
    let identity = Identity {
        actor: actor(&signer),
        author: None,
        sign: &|payload| signer.sign(payload),
    };
    let (target, outcome) = review::new(
        &root.refs,
        &root.objects,
        &root.events,
        &root.path,
        new,
        &member,
        &identity,
        root.mode(),
    )?;
    outcome_to_result(outcome, None)?;
    Ok(target)
}

/// `git ents review withdraw`: retract the signer's own review of `target`,
/// resolving the reviewer's member id exactly as [`new`] does — the
/// withdrawing member is always the signing identity's own resolved
/// member, never one named on the command line, so this can never be
/// pointed at someone else's review (`gate.owner-mutation` refuses it even
/// if it were).
///
/// # Errors
///
/// [`crate::error::Error::Forge`] (wrapping [`ents_forge::Error::NotFound`])
/// if this member has no existing review reaching `target`; otherwise as
/// [`new`].
pub fn withdraw(root: &LocalRoot, target: String, key: Option<std::path::PathBuf>) -> Result<String> {
    let signer = signer(root, key)?;
    let member = reviewer_member_id(root, &signer)?;
    let identity = Identity {
        actor: actor(&signer),
        author: None,
        sign: &|payload| signer.sign(payload),
    };
    let (target, outcome) = review::withdraw(
        &root.refs,
        &root.objects,
        &root.events,
        &root.path,
        &target,
        &member,
        &identity,
        root.mode(),
    )?;
    outcome_to_result(outcome, None)?;
    Ok(target)
}

/// The member id owning the signer's key — the composite review key's
/// `<member>` segment — via the same key-to-member scan
/// [`super::members::find_by_key`] already performs for `git ents members
/// check`. When the signing key enrolls no member (`roots.local`'s
/// advisory gate never requires enrollment before a local mutation lands),
/// falls back to the same fingerprint-derived placeholder [`super::actor`]
/// already uses for its own commit signature: a composite review key still
/// needs *some* member segment, and `gate.owner-mutation` — not this
/// fallback — is what actually keys ownership once a real deployment's
/// mandatory gate is in force.
///
/// # Errors
///
/// Propagates a ref-store or object read failure.
fn reviewer_member_id(root: &LocalRoot, signer: &crate::sign::Signer) -> Result<MemberId> {
    let pubkey = signer.public_openssh();
    if let Some((username, _state)) =
        super::members::find_by_key(&root.refs, &root.objects, &pubkey)?
    {
        return Ok(MemberId::new(username));
    }
    Ok(MemberId::new(super::short_fingerprint(signer)))
}

/// `git ents review list [--target rev]`: every review recorded in this
/// repository, keyed by its composite `(target, member)` segments,
/// optionally filtered to those reviewing `target`.
///
/// # Errors
///
/// Propagates a ref-store, object read, or revision-resolution failure.
pub fn list(root: &LocalRoot, target: Option<String>) -> Result<Vec<((String, MemberId), Review)>> {
    Ok(review::list(
        &root.refs,
        &root.objects,
        &root.path,
        target.as_deref(),
    )?)
}

/// `git ents review show`: `target`/`member`'s review, plus its discussion
/// thread.
///
/// # Errors
///
/// [`crate::error::Error::Forge`] (wrapping [`ents_forge::Error::NotFound`])
/// if `target`/`member` has no review ref.
pub fn show(
    root: &LocalRoot,
    target: &str,
    member: &str,
) -> Result<(Review, Vec<(String, Comment)>)> {
    Ok(review::show(
        &root.refs,
        &root.objects,
        target,
        &MemberId::new(member),
    )?)
}
