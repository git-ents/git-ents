//! `git ents comment`: a thin wrapper around `ents_forge::comment`'s
//! business logic — this module only resolves the signer/actor identity
//! against [`LocalRoot`] and translates a reached `Outcome` into a
//! CLI-facing [`Result`] (`crate::mutate::outcome_to_result`), exactly as
//! every other mutation command does.

use ents_forge::comment;
use ents_forge::comment::Comment;
use ents_receive::Identity;

use super::{actor, signer};
use crate::error::Result;
use crate::mutate::outcome_to_result;
use crate::root::LocalRoot;

/// `git ents comment list`: every comment recorded in this repository.
///
/// # Errors
///
/// Propagates a ref-store or object read failure.
pub fn list(root: &LocalRoot) -> Result<Vec<(String, Comment)>> {
    Ok(comment::list(&root.refs, &root.objects)?)
}

/// `git ents comment add`: anchor `body` to `path` (optionally `lines`) at
/// `rev`.
///
/// # Errors
///
/// [`crate::error::Error::Forge`] if `lines` does not parse, or anchoring,
/// serialization, or `receive` itself fails; see
/// [`crate::mutate::outcome_to_result`] for how a reached refusal renders.
pub fn add(
    root: &LocalRoot,
    path: &str,
    body: String,
    lines: Option<String>,
    rev: &str,
    key: Option<std::path::PathBuf>,
) -> Result<String> {
    let signer = signer(root, key)?;
    let identity = Identity {
        actor: actor(&signer),
        sign: &|payload| signer.sign(payload),
    };
    let (id, outcome) = comment::add(
        &root.refs,
        &root.objects,
        &root.events,
        &root.path,
        path,
        body,
        lines,
        rev,
        &identity,
        root.mode(),
    )?;
    outcome_to_result(outcome, None)?;
    Ok(id)
}

/// `git ents comment show`: `id`'s anchor (projected onto `rev`), anchored
/// text, and body.
///
/// # Errors
///
/// [`crate::error::Error::Forge`] (wrapping [`ents_forge::Error::NotFound`])
/// if `id` has no comment ref.
pub fn show(
    root: &LocalRoot,
    id: &str,
    rev: &str,
) -> Result<(Comment, ents_anchor::Anchor, ents_anchor::Projection)> {
    Ok(comment::show(
        &root.refs,
        &root.objects,
        &root.path,
        id,
        rev,
    )?)
}
