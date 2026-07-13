//! `git ents review`: a thin wrapper around `ents_forge::review`'s
//! business logic — this module only resolves the signer/actor identity
//! against [`LocalRoot`] and translates a reached `Outcome` into a
//! CLI-facing [`Result`] (`crate::mutate::outcome_to_result`), exactly as
//! every other mutation command does. Every operation is the library call
//! itself (`lens.parity`); nothing here re-implements one.

use ents_forge::comment::Comment;
use ents_forge::review;
use ents_forge::review::{NewReview, Review};
use ents_receive::Identity;

use super::{actor, signer};
use crate::error::Result;
use crate::mutate::outcome_to_result;
use crate::root::LocalRoot;

/// `git ents review new`: review a commit, writing both its entity ref and
/// its retention pin.
///
/// # Errors
///
/// [`crate::error::Error::Forge`] if `new.target` does not resolve to a
/// commit, or serialization or `receive` itself fails for either ref; see
/// [`crate::mutate::outcome_to_result`] for how a reached refusal renders.
pub fn new(root: &LocalRoot, new: NewReview, key: Option<std::path::PathBuf>) -> Result<String> {
    let signer = signer(root, key)?;
    let identity = Identity {
        actor: actor(&signer),
        sign: &|payload| signer.sign(payload),
    };
    let (id, outcome) = review::new(
        &root.refs,
        &root.objects,
        &root.events,
        &root.path,
        new,
        &identity,
        root.mode(),
    )?;
    outcome_to_result(outcome, None)?;
    Ok(id)
}

/// `git ents review list [--target rev]`: every review recorded in this
/// repository, optionally filtered to those reviewing `target`.
///
/// # Errors
///
/// Propagates a ref-store, object read, or revision-resolution failure.
pub fn list(root: &LocalRoot, target: Option<String>) -> Result<Vec<(String, Review)>> {
    Ok(review::list(
        &root.refs,
        &root.objects,
        &root.path,
        target.as_deref(),
    )?)
}

/// `git ents review show`: `id`'s review, plus its discussion thread.
///
/// # Errors
///
/// [`crate::error::Error::Forge`] (wrapping [`ents_forge::Error::NotFound`])
/// if `id` has no review ref.
pub fn show(root: &LocalRoot, id: &str) -> Result<(Review, Vec<(String, Comment)>)> {
    Ok(review::show(&root.refs, &root.objects, id)?)
}
