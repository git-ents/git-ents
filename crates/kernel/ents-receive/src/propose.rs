//! One shared primitive every entity-mutation command uses: serialize a
//! typed tree, wrap it in a signed commit bound to its refname (per
//! `meta-ref.trailers`'s `Advance-ref` trailer), and hand it to
//! [`crate::receive`] — the sole path a meta-ref mutation may enter the
//! repository (`receive.unit`).
//!
//! Every porcelain command that writes an entity (`members`, `account`,
//! `effect`, `toolchain`, `comment`, `redact`) goes through
//! [`propose_entity`] rather than repeating this shape, so there is exactly
//! one place that builds the trailer block, one place that signs, and one
//! place that calls `receive`. This lives in `ents-receive` rather than a
//! frontend crate because it is mechanism, not policy: every mutation
//! frontend across every composition root shares it (`arch.no-object-store-trait`'s
//! sibling rule — signing and commit-building plumbing is kernel material).

use ents_model::trailer::Trailers;
use gix::refs::FullName;
use gix_object::{Commit, Find, Kind, Write, WriteTo as _};
use gix_ref_store::RefStore;

use crate::error::Result;
use crate::outcome::{Mode, Outcome};
use crate::proposal::{Proposal, RefTransition};
use crate::sink::EventSink;

/// Everything [`propose_entity`] needs about the acting identity: the
/// commit author/committer signature and a signing function producing the
/// `gpgsig` header's armored payload.
pub struct Identity<'a> {
    /// The author and committer signature every mutation commit carries.
    pub actor: gix::actor::Signature,
    /// Signs a commit payload, returning the armored SSHSIG PEM block git
    /// stores in the `gpgsig` header — a closure rather than a concrete
    /// signer type so this crate never depends on how a caller loads or
    /// holds its signing key (a CLI's on-disk key, a test fixture's
    /// deterministic keypair, ...).
    pub sign: &'a dyn Fn(&[u8]) -> String,
}

/// Serialize `entity` into `objects`, wrap it in a commit bound to `name`
/// via the `Advance-ref` trailer, sign it with `identity`, and propose the
/// transition through [`crate::receive`].
///
/// `name`'s current tip is read fresh from `refs` immediately before
/// building the commit, so the proposed transition's `old` is always
/// current — the CAS precondition `receive` (via `ents_gate::verify`)
/// checks is against this same read.
///
/// # Errors
///
/// [`crate::Error::Tree`] if `entity` cannot be serialized; [`crate::Error::Refs`]
/// if reading `name`'s current tip fails; other [`crate::Error`] variants if
/// `receive` itself could not reach an outcome. A reached-but-negative
/// outcome (refusal, staleness, redaction) is returned as `Ok` — callers
/// translate [`Outcome`] to their own user-facing error (the CLI's
/// `outcome_to_result`, for instance).
///
/// # Examples
///
/// ```
/// use ents_model::{Provenance, Redaction, namespace};
/// use ents_receive::{Identity, Mode, NullEventSink, TxResult, propose_entity};
/// use ents_testutil::{Keypair, MemRefStore, ObjectStore, enroll_member};
///
/// let refs = MemRefStore::default();
/// let objects = ObjectStore::default();
/// let admin = Keypair::from_seed(1);
/// enroll_member(&refs, &objects, "admin", &admin, Provenance::AdminRegistered, 100);
///
/// let redaction = Redaction::new(
///     gix_hash::ObjectId::null(gix_hash::Kind::Sha1),
///     "leaked credential",
/// );
/// let name = namespace::redaction_ref("r1").expect("valid");
/// let identity = Identity {
///     actor: gix::actor::Signature {
///         name: "admin".into(),
///         email: "admin@ents.test".into(),
///         time: gix::date::Time { seconds: 300, offset: 0 },
///     },
///     sign: &|payload| admin.sign(payload),
/// };
///
/// let outcome = propose_entity(
///     &refs, &objects, &NullEventSink, name, &redaction, &identity, "Redact leaked secret",
///     Mode::Advisory,
/// )
/// .expect("reaches an outcome");
/// assert_eq!(outcome.result, TxResult::Applied);
/// ```
#[expect(
    clippy::too_many_arguments,
    reason = "one field per entity-mutation shape (refname, entity, identity, message, mode); \
              this is the crate's one shared primitive rather than one per caller"
)]
pub fn propose_entity<T: for<'facet> facet::Facet<'facet>>(
    refs: &dyn RefStore,
    objects: &(impl Find + Write),
    events: &dyn EventSink,
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
    let pem = (identity.sign)(&payload);
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
    crate::receive::receive(refs, objects, events, &proposal, mode)
}

/// Delete the entity at `name` (a `new: None` transition) through
/// `receive`, the same shared path [`propose_entity`] uses for writes.
///
/// # Errors
///
/// See [`propose_entity`].
///
/// # Examples
///
/// ```
/// use ents_model::{MemberId, Provenance, namespace};
/// use ents_receive::{Mode, NullEventSink, TxResult, propose_delete};
/// use ents_testutil::{Keypair, MemRefStore, ObjectStore, enroll_member};
/// use gix_ref_store::RefStoreRead as _;
///
/// let refs = MemRefStore::default();
/// let objects = ObjectStore::default();
/// let admin = Keypair::from_seed(1);
/// enroll_member(&refs, &objects, "admin", &admin, Provenance::AdminRegistered, 100);
///
/// let name = namespace::member_ref(&MemberId::new("admin")).expect("valid");
/// let outcome = propose_delete(&refs, &objects, &NullEventSink, name.clone(), Mode::Advisory)
///     .expect("reaches an outcome");
/// assert_eq!(outcome.result, TxResult::Applied);
/// assert_eq!(refs.get(name.as_ref()).expect("readable"), None);
/// ```
pub fn propose_delete(
    refs: &dyn RefStore,
    objects: &(impl Find + Write),
    events: &dyn EventSink,
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
    crate::receive::receive(refs, objects, events, &proposal, mode)
}
