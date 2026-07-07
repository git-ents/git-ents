//! Attested push (`docs/scale-out.adoc`, "Attested push"): uniform-strong
//! attestation on the way in, and the server-signed op record on the way
//! out.
//!
//! Signature verification against the enrolled member set is
//! `git-signed-push`'s [`git_signed_push::authorize`] — this module calls
//! it rather than re-implementing it, so the native ingest path and the
//! `pre-receive` hook enforce the identical policy. What's new here is the
//! op record: a server-signed git commit, chained under
//! `refs/meta/ops/log`, recording the push's intent (the client's
//! certificate, embedded by OID) and outcome (the applied ref edits).

use std::path::PathBuf;
use std::process::Command;

use git_backend::{ObjectStore, RefName};
use git_member::members::Member;
use gix_hash::ObjectId;
use gix_object::WriteTo as _;
use gix_object::bstr::BString;

use crate::pack::PackObject;
use crate::types::{AppliedRefEdit, PushCertificate};
use crate::{Error, Result};

/// The ref every op record is chained under (a linear history, oldest
/// parent-most): `docs/scale-out.adoc`'s audit trail for "the applied ref
/// edits" and "the client push certificate ... embedded by OID."
pub const OP_LOG_REF: &str = "refs/meta/ops/log";

/// The `ssh-keygen -Y sign -n <namespace>` namespace an op record's
/// signature is scoped to, mirroring how push certificates are scoped to
/// `-n git`.
const OP_RECORD_NAMESPACE: &str = "git-ents-op";

/// The server's op-log commit identity (`docs/scale-out.adoc` doesn't name
/// one; chosen to match `git-store`'s convention of a fixed system identity
/// for writes the server itself makes).
const IDENTITY_NAME: &str = "git-ents op-log";
const IDENTITY_EMAIL: &str = "op-log@git-ents";

/// How strongly a namespace requires a push to be attested. Currently a
/// single variant: `docs/scale-out.adoc`'s "Namespace attestation policy" is
/// pinned to `client-cert-required` everywhere, with the enum kept open (and
/// [`Ord`] derived, so [`max_over`] is a real max rather than a placeholder)
/// for the day a tiered policy is reintroduced — see the doc's decision
/// record on uniform-strong vs. tiered attestation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AttestationLevel {
    /// Every push must carry a client-signed push certificate that
    /// verifies against an enrolled member's key.
    ClientCertRequired,
}

/// The attestation level required for `namespace`. Pinned to
/// [`AttestationLevel::ClientCertRequired`] everywhere: no config plumbing,
/// per the doc's instruction that the per-namespace knob exists so
/// reversing uniform-strong is configuration later, not a schema change now.
#[must_use]
pub fn required_level(_namespace: &RefName) -> AttestationLevel {
    AttestationLevel::ClientCertRequired
}

/// The attestation level a push touching every ref in `ref_names` must meet:
/// the max of [`required_level`] over all of them, per "a push is evaluated
/// at the max level over all namespaces it touches."
#[must_use]
pub fn max_over<'a>(ref_names: impl IntoIterator<Item = &'a RefName>) -> AttestationLevel {
    ref_names
        .into_iter()
        .map(required_level)
        .max()
        .unwrap_or(AttestationLevel::ClientCertRequired)
}

/// Verify `cert` against `members` (already revocation-filtered) and
/// `config`'s per-role rules for every ref in `ref_names`, delegating to
/// [`git_signed_push::authorize`] so this is the same check the
/// `pre-receive` hook makes. Returns the identified signer, or `None` only
/// during the bootstrap window (`members` empty).
pub fn verify(
    members: Vec<Member>,
    config: &git_ents_core::config::Config,
    cert: Option<&PushCertificate>,
    ref_names: &[&str],
) -> Result<Option<Member>> {
    git_signed_push::authorize(
        members,
        config,
        cert.map(|cert| cert.raw.as_str()),
        ref_names,
    )
    .map_err(Error::Attestation)
}

/// Signs an op record's payload with the server's own key. `docs/
/// scale-out.adoc` calls the op record "server-signed" without prescribing
/// a mechanism; this mirrors the push certificate's own SSH-signature
/// convention (`ssh-keygen -Y sign`/`-Y verify`) rather than inventing a
/// second one.
pub trait OpSigner: Send + Sync {
    /// Sign `payload`, returning the armored SSH signature block.
    fn sign(&self, payload: &[u8]) -> Result<String>;
}

/// An [`OpSigner`] that shells out to `ssh-keygen -Y sign` with the server's
/// private key at `key_path`, exactly as `git-ents`'s own CLI signs a login
/// challenge (see `sign_challenge` in `crates/git-ents/src/main.rs`).
pub struct SshOpSigner {
    key_path: PathBuf,
}

impl SshOpSigner {
    /// Sign with the private key at `key_path`.
    pub fn new(key_path: impl Into<PathBuf>) -> Self {
        Self {
            key_path: key_path.into(),
        }
    }
}

impl OpSigner for SshOpSigner {
    fn sign(&self, payload: &[u8]) -> Result<String> {
        let dir = tempfile::tempdir()?;
        let data_path = dir.path().join("op-record");
        std::fs::write(&data_path, payload)?;
        let status = Command::new("ssh-keygen")
            .args(["-Y", "sign", "-f"])
            .arg(&self.key_path)
            .args(["-n", OP_RECORD_NAMESPACE])
            .arg(&data_path)
            .status()?;
        if !status.success() {
            return Err(Error::Attestation(
                "server could not sign the op record".to_owned(),
            ));
        }
        Ok(std::fs::read_to_string(dir.path().join("op-record.sig"))?)
    }
}

/// Build the server-signed op record for one accepted push: a git commit
/// over the well-known empty tree, chained onto `prev` (the current tip of
/// [`OP_LOG_REF`], if any), carrying the client's certificate (by OID) and
/// the applied ref edits as extra headers, and signed by `signer`.
///
/// Returns the record's own object id (the push id) and the objects that
/// must be staged for it to exist — the commit, plus the empty tree only if
/// `store` doesn't already have it.
pub fn build_op_record(
    prev: Option<ObjectId>,
    push_cert_oid: ObjectId,
    applied: &[AppliedRefEdit],
    signer: &dyn OpSigner,
    store: &dyn ObjectStore,
) -> Result<(ObjectId, Vec<PackObject>)> {
    let empty_tree = gix_hash::ObjectId::empty_tree(gix_hash::Kind::Sha1);
    let null = gix_hash::ObjectId::null(gix_hash::Kind::Sha1);

    let time = gix_date::Time::now_utc();
    let identity = gix_actor::Signature {
        name: IDENTITY_NAME.into(),
        email: IDENTITY_EMAIL.into(),
        time,
    };
    let mut extra_headers: Vec<(BString, BString)> = vec![(
        "push-cert".into(),
        push_cert_oid.to_hex().to_string().into(),
    )];
    for edit in applied {
        extra_headers.push((
            "ref-edit".into(),
            format!(
                "{} {} {}",
                edit.name,
                edit.old.unwrap_or(null),
                edit.new.unwrap_or(null)
            )
            .into(),
        ));
    }

    let mut commit = gix_object::Commit {
        tree: empty_tree,
        parents: prev.into_iter().collect(),
        author: identity.clone(),
        committer: identity,
        encoding: None,
        message: format!("push: {} ref edit(s)", applied.len()).into(),
        extra_headers,
    };

    let mut unsigned = Vec::new();
    commit
        .write_to(&mut unsigned)
        .map_err(|error| Error::Pack(error.to_string()))?;
    let signature = signer.sign(&unsigned)?;
    commit
        .extra_headers
        .push(("gpgsig".into(), signature.into()));

    let mut signed = Vec::new();
    commit
        .write_to(&mut signed)
        .map_err(|error| Error::Pack(error.to_string()))?;
    let oid = gix_object::compute_hash(gix_hash::Kind::Sha1, gix_object::Kind::Commit, &signed)
        .map_err(|error| Error::Pack(error.to_string()))?;

    let mut objects = Vec::new();
    if !store.contains(empty_tree)? {
        objects.push(PackObject {
            id: empty_tree,
            kind: gix_object::Kind::Tree,
            data: Vec::new(),
        });
    }
    objects.push(PackObject {
        id: oid,
        kind: gix_object::Kind::Commit,
        data: signed,
    });
    Ok((oid, objects))
}

/// The current tip of [`OP_LOG_REF`], the chain's `prev` for the next
/// record.
pub fn op_log_tip(refs: &dyn git_backend::RefStore) -> Result<Option<ObjectId>> {
    Ok(refs.get(&RefName::new(OP_LOG_REF))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_over_is_pinned_to_client_cert_required() {
        let refs = [RefName::new("refs/heads/main"), RefName::new("refs/meta/x")];
        assert_eq!(max_over(refs.iter()), AttestationLevel::ClientCertRequired);
    }

    #[test]
    fn max_over_empty_defaults_to_client_cert_required() {
        assert_eq!(
            max_over(std::iter::empty::<&RefName>()),
            AttestationLevel::ClientCertRequired
        );
    }
}
