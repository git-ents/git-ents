//! The fourth argument's shape (`receive.proposal-shape`): the ref
//! transitions a caller proposes, the new objects that accompany them, and
//! any transport-auth evidence the frontend collected.

use gix::refs::FullName;
use gix_hash::ObjectId;

/// One proposed ref transition: `(refname, old-oid, new-oid)`, exactly the
/// triple `receive.proposal-shape` names.
///
/// `old` is the frontier the *proposal* claims — what a `git push` command
/// line reports as its own base, or what a local UI last read. [`crate::receive`]
/// never trusts it for admission — [`ents_gate::verify`] re-reads the actual
/// current tip itself (the same snapshot every other check uses) — nor does
/// it enforce it: unlike git's `receive-pack`, which refuses a push whose
/// old-oid is stale, a stale `old` whose `new` still descends from the real
/// tip applies cleanly, and one that does not is refused by the gate's own
/// fast-forward check against the re-read tip, never by comparing this field.
///
/// # Examples
///
/// ```
/// use ents_receive::RefTransition;
///
/// let transition = RefTransition {
///     name: "refs/meta/issues/1".try_into().expect("valid"),
///     old: None,
///     new: Some(gix_hash::ObjectId::null(gix_hash::Kind::Sha1)),
/// };
/// assert!(transition.old.is_none());
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefTransition {
    /// The ref being updated.
    pub name: FullName,
    /// The tip the proposal claims as its base, or `None` for creation.
    pub old: Option<ObjectId>,
    /// The proposed new tip, or `None` to delete the ref.
    pub new: Option<ObjectId>,
}

/// Transport-level authentication evidence a frontend collected: a
/// signed-push credential, a smart-HTTP session, or nothing for a frontend
/// whose transport carries no separate authentication (`receive.proposal-shape`).
///
/// This is a connection-level ACL input for `refs/heads/*` only
/// (`gate.principled-split`) — no such ACL policy is defined yet anywhere in
/// the spec, so [`crate::receive`] accepts and threads this value through
/// without interpreting it; it is never substituted for the tip invariant on
/// a `refs/meta/*` update, which is the one thing `receive.proposal-shape`
/// actually requires today. Defining and enforcing the `refs/heads/*` ACL
/// itself is future work with no requirement id yet to hang it on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportAuth {
    /// Opaque evidence bytes: a signed-push certificate, a session token,
    /// or whatever shape a future frontend needs. `receive` never parses
    /// this; only a future `refs/heads/*` ACL check would.
    pub evidence: Vec<u8>,
}

/// The fourth argument to [`crate::receive`]: every proposed ref transition
/// this call attempts, the object ids the proposal introduces, and any
/// transport-auth evidence (`receive.proposal-shape`).
///
/// # Examples
///
/// ```
/// use ents_receive::{Proposal, RefTransition};
///
/// let proposal = Proposal {
///     transitions: vec![RefTransition {
///         name: "refs/meta/issues/1".try_into().expect("valid"),
///         old: None,
///         new: Some(gix_hash::ObjectId::null(gix_hash::Kind::Sha1)),
///     }],
///     objects: vec![gix_hash::ObjectId::null(gix_hash::Kind::Sha1)],
///     auth: None,
/// };
/// assert_eq!(proposal.transitions.len(), 1);
/// ```
// @relation(receive.proposal-shape, scope=file)
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Proposal {
    /// The proposed ref transitions this call attempts, applied together
    /// as one atomic batch (`arch.refstore-read-cas-split`,
    /// `gate.atomic-cas`).
    pub transitions: Vec<RefTransition>,
    /// The object ids this proposal introduces, already durably present in
    /// the object store [`crate::receive`] is handed (the frontend's job
    /// per `receive.shared-path`: the CLI and local UI write directly, and
    /// smart-HTTP unpacks the incoming pack, before `receive` is ever
    /// called). `receive` checks each of these against the redaction list
    /// at ingest time (`receive.redaction-ingest`).
    pub objects: Vec<ObjectId>,
    /// Transport-auth evidence the frontend collected, or `None`. See
    /// [`TransportAuth`] for why `receive` does not interpret this today.
    pub auth: Option<TransportAuth>,
}
