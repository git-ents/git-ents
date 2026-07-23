//! One shared primitive every entity-mutation command uses: serialize a
//! typed tree, wrap it in a signed commit, and hand it to
//! [`crate::receive`] — the sole path a meta-ref mutation may enter the
//! repository (`receive.unit`). No commit trailer binds the commit to its
//! refname anymore: the gate recomputes the refname from the signed
//! content (`meta-ref.identity-binding`, `gate.identity-binding`).
//!
//! An owner-keyed mutation advances an existing ref, whose name the caller
//! already knows ([`propose_entity`]). The creation of a hash-identified
//! entity instead runs sign-then-name ([`propose_genesis`]): build and
//! sign the genesis commit first, then name the ref from that commit's own
//! oid — there is no circularity, because no commit names its own ref.
//!
//! Every porcelain command that writes an entity (`members`, `account`,
//! `effect`, `toolchain`, `comment`, `redact`) goes through one of these
//! rather than repeating the shape, so there is exactly one place that
//! signs and one place that calls `receive`. This lives in `ents-receive`
//! rather than a frontend crate because it is mechanism, not policy: every
//! mutation frontend across every composition root shares it
//! (`arch.no-object-store-trait`'s sibling rule — signing and
//! commit-building plumbing is kernel material).
//!
//! [`propose_pin`] is the same mechanism for the one commit shape that
//! carries no entity: a retention pin's empty-tree, merge-shaped commit
//! (`model.review-pin`), built and signed by the identical
//! [`signed_commit`] plumbing and admitted through the identical gate.

use gix::refs::FullName;
use gix_object::{Commit, Find, Kind, Write, WriteTo as _};
use gix_ref_store::RefStore;

use crate::error::Result;
use crate::outcome::{Mode, Outcome};
use crate::proposal::{Proposal, RefTransition};
use crate::sink::EventSink;

/// Everything [`propose_entity`] needs about the acting identity: the
/// commit committer signature, an optional attributed author, and a
/// signing function producing the `gpgsig` header's armored payload.
// @relation(receive.attributed-author, scope=type)
pub struct Identity<'a> {
    /// The committer signature every mutation commit carries — always the
    /// signing identity (`receive.attributed-author`).
    pub actor: gix::actor::Signature,
    /// A distinct attributed author, so history reads "member via the
    /// web" (`receive.attributed-author`, `roots.web-signing`); `None`
    /// means the author is `actor`, the unattributed common case. The
    /// gate keys authorization off the signer, never this field.
    pub author: Option<gix::actor::Signature>,
    /// Signs a commit payload, returning the armored SSHSIG PEM block git
    /// stores in the `gpgsig` header — a closure rather than a concrete
    /// signer type so this crate never depends on how a caller loads or
    /// holds its signing key (a CLI's on-disk key, a test fixture's
    /// deterministic keypair, ...).
    pub sign: &'a dyn Fn(&[u8]) -> String,
}

/// Serialize `entity` into `objects`, wrap it in a signed commit whose
/// only parent is `name`'s current tip, and propose the transition through
/// [`crate::receive`]. This is the owner-keyed advance of an existing ref
/// (`members`, `account`, `effect`, ...); the ref name is bound to the
/// signed content by the gate (`gate.identity-binding`), not a trailer.
/// For the creation of a hash-identified entity, use [`propose_genesis`].
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
///     author: None,
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
    let (transition, tip) = entity_transition(refs, objects, &name, entity, identity, subject)?;
    let proposal = Proposal {
        transitions: vec![transition],
        objects: vec![tip],
        auth: None,
    };
    crate::receive::receive(refs, objects, events, &proposal, mode)
}

/// Build the entity-mutation [`RefTransition`] `propose_entity` and
/// `propose_entity_with_pin` share: serialize `entity`, wrap it in a signed
/// commit whose only parent is `name`'s current tip, and return that
/// transition alongside the tip oid the [`Proposal`] must carry.
///
/// Public so a caller assembling a larger, bespoke atomic multi-ref
/// proposal (`receive.multi-ref-atomicity`) than [`propose_entity_with_pin`]
/// covers can build one of its transitions with the identical signing
/// plumbing every entity mutation uses, then bundle it into its own
/// [`Proposal`] and call [`crate::receive`] exactly once. This does not
/// widen `receive`'s own contract: the transition still only becomes
/// durable through that one call.
pub fn entity_transition<T: for<'facet> facet::Facet<'facet>>(
    refs: &dyn RefStore,
    objects: &(impl Find + Write),
    name: &FullName,
    entity: &T,
    identity: &Identity<'_>,
    subject: &str,
) -> Result<(RefTransition, gix_hash::ObjectId)> {
    let tree = facet_git_tree::serialize_into(entity, objects)?;
    let old = refs.get(name.as_ref())?;
    let parents: Vec<_> = old.into_iter().collect();
    let tip = signed_commit(objects, tree, parents, identity, subject)?;
    Ok((
        RefTransition {
            name: name.clone(),
            old,
            new: Some(tip),
        },
        tip,
    ))
}

/// Create a hash-identified entity by sign-then-name (`model.comment`,
/// `model.issue`, `meta-ref.identity-binding`): serialize `entity`, build
/// and sign a *parentless* genesis commit, then name the ref from that
/// commit's own oid via `name_from_oid` (for example
/// `ents_model::namespace::comment_ref`) and propose the creation through
/// [`crate::receive`]. Returns the derived refname alongside the outcome,
/// since the caller cannot know the id until the commit is signed.
///
/// There is no circularity — the genesis commit carries no reference to
/// the ref it will name (no trailer names it) — so the id is git's own
/// hash over the genesis tree, author, timestamp, and signature, exactly
/// as `model.comment` requires, and the gate's all-roots walk binds it
/// (`gate.identity-binding`).
///
/// # Errors
///
/// [`crate::Error::Tree`] if `entity` cannot be serialized;
/// [`crate::Error::Refs`] if building the refname fails (an invalid oid
/// segment cannot occur for a real oid, but the closure is fallible);
/// other [`crate::Error`] variants if `receive` could not reach an
/// outcome.
///
/// # Examples
///
/// ```
/// use ents_model::{Provenance, namespace};
/// use ents_receive::{Identity, Mode, NullEventSink, TxResult, propose_genesis};
/// use ents_testutil::{Keypair, MemRefStore, ObjectStore, enroll_member};
/// use gix_ref_store::RefStoreRead as _;
///
/// # #[derive(facet::Facet)]
/// # struct Comment { body: String }
/// let refs = MemRefStore::default();
/// let objects = ObjectStore::default();
/// let admin = Keypair::from_seed(1);
/// enroll_member(&refs, &objects, "admin", &admin, Provenance::AdminRegistered, 100);
///
/// let identity = Identity {
///     actor: gix::actor::Signature {
///         name: "admin".into(),
///         email: "admin@ents.test".into(),
///         time: gix::date::Time { seconds: 300, offset: 0 },
///     },
///     author: None,
///     sign: &|payload| admin.sign(payload),
/// };
///
/// let (name, outcome) = propose_genesis(
///     &refs, &objects, &NullEventSink, &Comment { body: "first".into() },
///     |oid| namespace::comment_ref(&oid.to_string()), &identity, "Comment on X",
///     Mode::Advisory,
/// )
/// .expect("reaches an outcome");
/// assert_eq!(outcome.result, TxResult::Applied);
/// // The ref is named from the genesis commit's own oid.
/// assert!(name.as_bstr().starts_with(b"refs/meta/comments/"));
/// assert!(refs.get(name.as_ref()).expect("read").is_some());
/// ```
// @relation(meta-ref.identity-binding, model.comment, model.issue, scope=function)
#[expect(
    clippy::too_many_arguments,
    reason = "mirrors propose_entity's shape, plus the name-from-oid closure that is the whole \
              point of the sign-then-name flow"
)]
pub fn propose_genesis<T: for<'facet> facet::Facet<'facet>>(
    refs: &dyn RefStore,
    objects: &(impl Find + Write),
    events: &dyn EventSink,
    entity: &T,
    name_from_oid: impl FnOnce(gix_hash::ObjectId) -> ents_model::Result<FullName>,
    identity: &Identity<'_>,
    subject: &str,
    mode: Mode,
) -> Result<(FullName, Outcome)> {
    let tree = facet_git_tree::serialize_into(entity, objects)?;
    let tip = signed_commit(objects, tree, Vec::new(), identity, subject)?;
    let name = name_from_oid(tip).map_err(|source| crate::Error::Model { source })?;
    let proposal = Proposal {
        transitions: vec![RefTransition {
            name: name.clone(),
            old: None,
            new: Some(tip),
        }],
        objects: vec![tip],
        auth: None,
    };
    let outcome = crate::receive::receive(refs, objects, events, &proposal, mode)?;
    Ok((name, outcome))
}

/// Create a hash-identified entity whose genesis commit carries `retain` as
/// its parents, the claim-creation path: sign-then-name exactly as
/// [`propose_genesis`], except the genesis is not parentless. This
/// generalizes [`propose_pin`]'s retention linkage (`model.review-pin`) to
/// an entity-carrying commit — a claim's binding supplies its own witness
/// commits as `retain`, so the claim's own ledger commit keeps the bound
/// objects reachable without a separate pin ref. The only difference from
/// [`propose_genesis`] is that parent list; [`signed_commit`] is reused
/// unchanged.
///
/// There is no "advance" counterpart: a claim ref is append-once (the tip
/// IS the genesis), so unlike [`propose_entity`] or [`propose_pin`], no
/// second function exists here to move an existing ref forward — a changed
/// assertion is a new claim, proposed fresh through this same function.
///
/// # Errors
///
/// See [`propose_genesis`] — identical.
///
/// # Examples
///
/// ```
/// use ents_model::{Claim, MemberId, Provenance, claim::Verdict, namespace};
/// use ents_receive::{Identity, Mode, NullEventSink, TxResult, propose_genesis_retaining};
/// use ents_testutil::{CommitSpec, Keypair, MemRefStore, ObjectStore, enroll_member, write_commit};
/// use gix_ref_store::RefStoreRead as _;
///
/// let refs = MemRefStore::default();
/// let objects = ObjectStore::default();
/// let admin = Keypair::from_seed(1);
/// enroll_member(&refs, &objects, "admin", &admin, Provenance::AdminRegistered, 100);
///
/// // The commit under claim — the content the claim keeps reachable.
/// let tree = ents_testutil::empty_tree(&objects);
/// let witness = write_commit(
///     &objects,
///     &CommitSpec { tree, parents: vec![], message: "witnessed work".into(), seconds: 200 },
///     None,
/// );
///
/// let binding = ents_anchor::Binding::Commit { commit: witness };
/// let claim = Claim::new(MemberId::new("admin"), &binding, Verdict::Affirm, "review", &objects)
///     .expect("serialize binding");
///
/// let identity = Identity {
///     actor: gix::actor::Signature {
///         name: "admin".into(),
///         email: "admin@ents.test".into(),
///         time: gix::date::Time { seconds: 300, offset: 0 },
///     },
///     author: None,
///     sign: &|payload| admin.sign(payload),
/// };
///
/// let (name, outcome) = propose_genesis_retaining(
///     &refs, &objects, &NullEventSink, &claim, &[witness],
///     |oid| namespace::claim_ref(&oid.to_string()), &identity, "Claim on witness",
///     Mode::Advisory,
/// )
/// .expect("reaches an outcome");
/// assert_eq!(outcome.result, TxResult::Applied);
/// let tip = refs.get(name.as_ref()).expect("read").expect("ref exists");
/// let stored = objects.get(&tip).expect("commit stored");
/// let gix_object::Object::Commit(commit) = stored else { panic!("not a commit") };
/// assert!(commit.parents.iter().any(|parent| *parent == witness));
/// ```
#[expect(
    clippy::too_many_arguments,
    reason = "mirrors propose_genesis's shape, plus the retained-parents slice that is this \
              function's whole point"
)]
pub fn propose_genesis_retaining<T: for<'facet> facet::Facet<'facet>>(
    refs: &dyn RefStore,
    objects: &(impl Find + Write),
    events: &dyn EventSink,
    entity: &T,
    retain: &[gix_hash::ObjectId],
    name_from_oid: impl FnOnce(gix_hash::ObjectId) -> ents_model::Result<FullName>,
    identity: &Identity<'_>,
    subject: &str,
    mode: Mode,
) -> Result<(FullName, Outcome)> {
    let tree = facet_git_tree::serialize_into(entity, objects)?;
    let tip = signed_commit(objects, tree, retain.to_vec(), identity, subject)?;
    let name = name_from_oid(tip).map_err(|source| crate::Error::Model { source })?;
    let proposal = Proposal {
        transitions: vec![RefTransition {
            name: name.clone(),
            old: None,
            new: Some(tip),
        }],
        objects: vec![tip],
        auth: None,
    };
    let outcome = crate::receive::receive(refs, objects, events, &proposal, mode)?;
    Ok((name, outcome))
}

/// Advance the retention pin at `name` to keep `retain` (and its ancestry)
/// reachable (`model.review-pin`): a signed commit carrying the empty tree
/// — a pin's commits anchor other content's reachability and carry no
/// entity, the sole exception to `meta-ref.namespace`'s
/// tree-is-the-entity shape — whose parents are the pin's current tip (if
/// any) followed by `retain`, proposed through [`crate::receive`] exactly
/// like an entity mutation.
///
/// A first pin has `retain` as its only parent; every later advance is the
/// merge-shaped fast-forward `model.review-pin` requires (previous pin
/// tip, newly retained commit), so every retained round stays in the
/// pin's own history and the gate's descent check (`gate.fast-forward`,
/// descent through *any* parent) admits it unchanged.
///
/// # Errors
///
/// See [`propose_entity`] — identical, minus the serialization failure a
/// pin cannot have (there is no entity to serialize).
///
/// # Examples
///
/// ```
/// use ents_model::{MemberId, Provenance, namespace};
/// use ents_receive::{Identity, Mode, NullEventSink, TxResult, propose_pin};
/// use ents_testutil::{CommitSpec, Keypair, MemRefStore, ObjectStore, empty_tree, enroll_member};
/// use gix_object::Write as _;
///
/// let refs = MemRefStore::default();
/// let objects = ObjectStore::default();
/// let admin = Keypair::from_seed(1);
/// enroll_member(&refs, &objects, "admin", &admin, Provenance::AdminRegistered, 100);
///
/// // The commit under review — the content the pin keeps reachable.
/// let tree = empty_tree(&objects);
/// let reviewed = ents_testutil::write_commit(
///     &objects,
///     &CommitSpec { tree, parents: vec![], message: "reviewed work".into(), seconds: 200 },
///     None,
/// );
///
/// let name = namespace::review_pin_ref("7", &MemberId::new("admin")).expect("valid");
/// let identity = Identity {
///     actor: gix::actor::Signature {
///         name: "admin".into(),
///         email: "admin@ents.test".into(),
///         time: gix::date::Time { seconds: 300, offset: 0 },
///     },
///     author: None,
///     sign: &|payload| admin.sign(payload),
/// };
///
/// let outcome = propose_pin(
///     &refs, &objects, &NullEventSink, name, reviewed, &identity, "Pin review 7",
///     Mode::Advisory,
/// )
/// .expect("reaches an outcome");
/// assert_eq!(outcome.result, TxResult::Applied);
/// ```
// @relation(model.review-pin, meta-ref.namespace, scope=function)
#[expect(
    clippy::too_many_arguments,
    reason = "one field per pin-mutation shape (refname, retained commit, identity, message, \
              mode), mirroring propose_entity's identical, identically-justified shape"
)]
pub fn propose_pin(
    refs: &dyn RefStore,
    objects: &(impl Find + Write),
    events: &dyn EventSink,
    name: FullName,
    retain: gix_hash::ObjectId,
    identity: &Identity<'_>,
    subject: &str,
    mode: Mode,
) -> Result<Outcome> {
    let (transition, tip) = pin_transition(refs, objects, &name, retain, identity, subject)?;
    let proposal = Proposal {
        transitions: vec![transition],
        objects: vec![tip],
        auth: None,
    };
    crate::receive::receive(refs, objects, events, &proposal, mode)
}

/// Build the retention-pin [`RefTransition`] `propose_pin` and
/// `propose_entity_with_pin` share: an empty-tree, merge-shaped signed
/// commit whose parents are `name`'s current tip (if any) followed by
/// `retain` (`model.review-pin`), returned alongside the tip oid the
/// [`Proposal`] must carry.
fn pin_transition(
    refs: &dyn RefStore,
    objects: &(impl Find + Write),
    name: &FullName,
    retain: gix_hash::ObjectId,
    identity: &Identity<'_>,
    subject: &str,
) -> Result<(RefTransition, gix_hash::ObjectId)> {
    let tree = objects.write(&gix_object::Tree { entries: vec![] })?;
    let old = refs.get(name.as_ref())?;
    let parents: Vec<_> = old.into_iter().chain(std::iter::once(retain)).collect();
    let tip = signed_commit(objects, tree, parents, identity, subject)?;
    Ok((
        RefTransition {
            name: name.clone(),
            old,
            new: Some(tip),
        },
        tip,
    ))
}

/// Write an entity and its retention pin as one atomic mutation
/// (`receive.multi-ref-atomicity`): the entity commit at `entity_name` and
/// the empty-tree pin commit at `pin_name` (retaining `retain`) travel in a
/// single [`Proposal`] through one [`crate::receive`] call, so the
/// ref-store's atomic multi-ref compare-and-swap admits or refuses both
/// together — a review is never observable with its entity written but its
/// pin missing (`model.review`, `model.review-pin`).
///
/// # Errors
///
/// As [`propose_entity`] and [`propose_pin`], for either ref; a
/// reached-but-negative [`Outcome`] on either transition refuses the whole
/// batch and is returned as `Ok`.
///
/// # Examples
///
/// ```
/// use ents_model::{MemberId, Provenance, namespace};
/// use ents_receive::{Identity, Mode, NullEventSink, TxResult, propose_entity_with_pin};
/// use ents_testutil::{CommitSpec, Keypair, MemRefStore, ObjectStore, empty_tree, enroll_member};
/// use facet::Facet;
/// use gix_ref_store::RefStoreRead as _;
///
/// # #[derive(Facet)]
/// # struct Review { verdict: String }
/// let refs = MemRefStore::default();
/// let objects = ObjectStore::default();
/// let admin = Keypair::from_seed(1);
/// enroll_member(&refs, &objects, "admin", &admin, Provenance::AdminRegistered, 100);
///
/// let tree = empty_tree(&objects);
/// let reviewed = ents_testutil::write_commit(
///     &objects,
///     &CommitSpec { tree, parents: vec![], message: "reviewed work".into(), seconds: 200 },
///     None,
/// );
///
/// let identity = Identity {
///     actor: gix::actor::Signature {
///         name: "admin".into(),
///         email: "admin@ents.test".into(),
///         time: gix::date::Time { seconds: 300, offset: 0 },
///     },
///     author: None,
///     sign: &|payload| admin.sign(payload),
/// };
///
/// let member = MemberId::new("admin");
/// let outcome = propose_entity_with_pin(
///     &refs, &objects, &NullEventSink,
///     namespace::review_ref("7", &member).expect("valid"), &Review { verdict: "approve".into() },
///     namespace::review_pin_ref("7", &member).expect("valid"), reviewed,
///     &identity, "Review 7", "Pin review 7", Mode::Advisory,
/// )
/// .expect("reaches an outcome");
/// assert_eq!(outcome.result, TxResult::Applied);
/// // Both refs advanced together.
/// assert!(refs.get(namespace::review_ref("7", &member).expect("valid").as_ref()).expect("read").is_some());
/// assert!(refs.get(namespace::review_pin_ref("7", &member).expect("valid").as_ref()).expect("read").is_some());
/// ```
// @relation(receive.multi-ref-atomicity, model.review, model.review-pin, scope=function)
#[expect(
    clippy::too_many_arguments,
    reason = "one field per ref this entity spans (entity name+value, pin name+retained commit) \
              plus the shared identity/subjects/mode; the atomic counterpart of propose_entity \
              and propose_pin, which carry the same justification"
)]
pub fn propose_entity_with_pin<T: for<'facet> facet::Facet<'facet>>(
    refs: &dyn RefStore,
    objects: &(impl Find + Write),
    events: &dyn EventSink,
    entity_name: FullName,
    entity: &T,
    pin_name: FullName,
    retain: gix_hash::ObjectId,
    identity: &Identity<'_>,
    entity_subject: &str,
    pin_subject: &str,
    mode: Mode,
) -> Result<Outcome> {
    let (entity_transition, entity_tip) = entity_transition(
        refs,
        objects,
        &entity_name,
        entity,
        identity,
        entity_subject,
    )?;
    let (pin_transition, pin_tip) =
        pin_transition(refs, objects, &pin_name, retain, identity, pin_subject)?;
    let proposal = Proposal {
        transitions: vec![entity_transition, pin_transition],
        objects: vec![entity_tip, pin_tip],
        auth: None,
    };
    crate::receive::receive(refs, objects, events, &proposal, mode)
}

/// Build, sign, and write the commit every proposal shape shares: `tree`
/// under `subject`, authored and signed by `identity` — the one place a
/// commit is built, whatever its tree and parents. The commit carries no
/// reference to the ref it will name; the gate recomputes that name from
/// the signed content (`gate.identity-binding`), which is exactly what
/// lets [`propose_genesis`] name a ref from this commit's own oid without
/// circularity.
fn signed_commit(
    objects: &impl Write,
    tree: gix_hash::ObjectId,
    parents: Vec<gix_hash::ObjectId>,
    identity: &Identity<'_>,
    subject: &str,
) -> Result<gix_hash::ObjectId> {
    let message = subject.to_owned();

    let mut commit = Commit {
        tree,
        parents: parents.into(),
        author: identity
            .author
            .clone()
            .unwrap_or_else(|| identity.actor.clone()),
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
    Ok(objects.write_buf(Kind::Commit, &raw)?)
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
