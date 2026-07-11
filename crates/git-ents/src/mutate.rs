//! One shared primitive every entity-mutation command uses: serialize a
//! typed tree, wrap it in a signed commit bound to its refname (per
//! `meta-ref.trailers`'s `Advance-ref` trailer), and hand it to
//! [`ents_receive::receive`] — the sole path a meta-ref mutation may enter
//! the repository (`receive.unit`).
//!
//! Every porcelain command that writes an entity (`members`, `account`,
//! `effect`, `toolchain`, `comment`, `redact`) goes through
//! [`propose_entity`] rather than repeating this shape, so there is
//! exactly one place that builds the trailer block, one place that signs,
//! and one place that calls `receive`.

use ents_model::trailer::Trailers;
use ents_receive::{Mode, Outcome, Proposal, RefTransition, TxResult};
use gix::refs::FullName;
use gix_hash::ObjectId;
use gix_object::{Commit, Find, Kind, Write, WriteTo as _};
use gix_ref_store::RefStore;

use crate::error::{Error, Result};
use crate::sign::Signer;

/// Everything [`propose_entity`] needs about the acting identity: the
/// commit author/committer signature and the signer producing the
/// `gpgsig` header.
pub struct Identity<'a> {
    /// The author and committer signature every mutation commit carries.
    pub actor: gix::actor::Signature,
    /// The loaded signing key.
    pub signer: &'a Signer,
}

/// Serialize `entity` into `objects`, wrap it in a commit bound to `name`
/// via the `Advance-ref` trailer, sign it with `identity`, and propose the
/// transition through [`ents_receive::receive`].
///
/// `name`'s current tip is read fresh from `refs` immediately before
/// building the commit, so the proposed transition's `old` is always
/// current — the CAS precondition `receive` (via `ents_gate::verify`)
/// checks is against this same read.
///
/// # Errors
///
/// [`Error::Tree`] if `entity` cannot be serialized; [`Error::Refs`] if
/// reading `name`'s current tip fails; [`Error::Receive`] if `receive`
/// itself could not reach an outcome. A reached-but-negative outcome
/// (refusal, staleness, redaction) is returned as `Ok` — callers translate
/// [`Outcome`] to a user-facing [`Error`] via [`outcome_to_result`].
#[expect(
    clippy::too_many_arguments,
    reason = "one field per entity-mutation shape (refname, entity, identity, message, mode); \
              this is the crate's one shared primitive rather than one per caller"
)]
pub fn propose_entity<T: for<'facet> facet::Facet<'facet>>(
    refs: &dyn RefStore,
    objects: &(impl Find + Write),
    events: &dyn ents_receive::EventSink,
    name: FullName,
    entity: &T,
    identity: &Identity<'_>,
    subject: &str,
    mode: Mode,
) -> Result<Outcome> {
    let tree = facet_git_tree::serialize_into(entity, objects)?;
    let old = refs.get(name.as_ref())?;

    let trailers = Trailers {
        ents_ref: Some(name.clone()),
        schema_version: None,
    };
    let message = format!("{subject}\n\n{}", trailers.render());

    let mut commit = Commit {
        tree,
        parents: old.into_iter().collect::<Vec<_>>().into(),
        author: identity.actor.clone(),
        committer: identity.actor.clone(),
        encoding: None,
        message: message.into(),
        extra_headers: Vec::new(),
    };
    let mut payload = Vec::new();
    #[expect(
        clippy::expect_used,
        clippy::unwrap_in_result,
        reason = "writing a gix_object::Commit to an in-memory Vec cannot fail; mirrors \
                  `ents_testutil::write_commit`'s identical, unguarded call"
    )]
    commit
        .write_to(&mut payload)
        .expect("serializing a commit to a Vec cannot fail");
    let pem = identity.signer.sign(&payload);
    commit
        .extra_headers
        .push(("gpgsig".into(), pem.trim_end().into()));

    let mut raw = Vec::new();
    #[expect(
        clippy::expect_used,
        clippy::unwrap_in_result,
        reason = "writing a gix_object::Commit to an in-memory Vec cannot fail; mirrors \
                  `ents_testutil::write_commit`'s identical, unguarded call"
    )]
    commit
        .write_to(&mut raw)
        .expect("serializing a commit to a Vec cannot fail");
    let tip = objects.write_buf(Kind::Commit, &raw)?;

    let proposal = Proposal {
        transitions: vec![RefTransition {
            name,
            old,
            new: Some(tip),
        }],
        objects: vec![tip],
        auth: None,
    };
    Ok(ents_receive::receive(
        refs, objects, events, &proposal, mode,
    )?)
}

/// Delete the entity at `name` (a `new: None` transition) through
/// `receive`, the same shared path [`propose_entity`] uses for writes.
///
/// # Errors
///
/// See [`propose_entity`].
pub fn propose_delete(
    refs: &dyn RefStore,
    objects: &(impl Find + Write),
    events: &dyn ents_receive::EventSink,
    name: FullName,
    mode: Mode,
) -> Result<Outcome> {
    let old = refs.get(name.as_ref())?;
    let proposal = Proposal {
        transitions: vec![RefTransition {
            name,
            old,
            new: None,
        }],
        objects: vec![],
        auth: None,
    };
    Ok(ents_receive::receive(
        refs, objects, events, &proposal, mode,
    )?)
}

/// Translate a reached [`Outcome`] into `Ok(tip)` on success or a
/// user-facing [`Error`] otherwise — the one place every command renders
/// `receive`'s result the same way.
///
/// # Errors
///
/// [`Error::Refused`] for a gate refusal (`gate.mandatory-hosted`
/// aborting on any failed verdict, or an advisory root's failed verdict a
/// caller chose to treat as fatal); [`Error::Stale`] for a compare-and-swap
/// rejection; [`Error::Redacted`] if a redacted object was refused.
pub fn outcome_to_result(outcome: Outcome, tip: Option<ObjectId>) -> Result<Option<ObjectId>> {
    match outcome.result {
        TxResult::Applied => Ok(tip),
        TxResult::Refused => {
            let reasons = outcome
                .verdicts
                .iter()
                .filter_map(|(_, verdict)| match verdict {
                    ents_gate::Verdict::Fail(refusal) => Some(refusal.to_string()),
                    ents_gate::Verdict::Pass(_) => None,
                })
                .collect::<Vec<_>>()
                .join("; ");
            Err(Error::Refused(reasons))
        }
        TxResult::Rejected { name } => Err(Error::Stale {
            name: name.as_bstr().to_string(),
        }),
        TxResult::Redacted { oid } => Err(Error::Redacted { oid }),
    }
}
