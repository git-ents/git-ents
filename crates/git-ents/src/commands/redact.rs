//! `git ents redact`: record that an object was redacted
//! (`model.redaction`), refusing any future push that would refill it
//! (`receive.redaction-ingest`).

use ents_model::{Redaction, namespace};

use super::{actor, signer};
use crate::error::{Error, Result};
use crate::mutate::{Identity, outcome_to_result, propose_entity};
use crate::root::LocalRoot;

/// Run `git ents redact <oid> --reason ...`.
///
/// The record lands at `refs/meta/redactions/<id>`; the gate's default
/// namespace-authorization arm requires admin-registered provenance for
/// this namespace, so a non-admin signer is refused here exactly as any
/// other call site would refuse it (`gate.call-sites`,
/// `receive.redaction-admin-only`).
///
/// # Errors
///
/// [`Error::InvalidArgument`] if `oid` does not parse as an object id;
/// otherwise see [`crate::mutate::outcome_to_result`].
pub fn run(
    root: &LocalRoot,
    oid: &str,
    reason: String,
    key: Option<std::path::PathBuf>,
) -> Result<()> {
    let target: gix_hash::ObjectId = oid
        .parse()
        .map_err(|_source| Error::InvalidArgument(format!("not an object id: {oid}")))?;
    let redaction = Redaction::new(target, reason);
    let id = target.to_string();
    let ref_name = namespace::redaction_ref(&id)?;

    let signer = signer(root, key)?;
    let identity = Identity {
        actor: actor(&signer),
        signer: &signer,
    };
    let outcome = propose_entity(
        &root.refs,
        &root.objects,
        &root.events,
        ref_name,
        &redaction,
        &identity,
        &format!("Redact {id}"),
        root.mode(),
    )?;
    outcome_to_result(outcome, None)?;
    Ok(())
}
