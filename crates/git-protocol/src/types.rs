//! The minimal supporting types the protocol traits (`docs/scale-out.adoc`,
//! "Protocol traits") pass between a client and the server implementations
//! in [`crate::native`].

use git_backend::{PackStream, RefEdit, RefName};
use gix_hash::ObjectId;

/// Which repository a protocol call targets. Backends that serve more than
/// one repository (every backend that isn't a single-repo test fixture)
/// resolve this to a concrete [`git_backend::RefStore`]/[`git_backend::ObjectStore`]
/// pair — see [`crate::native::BackendResolver`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RepoId(String);

impl RepoId {
    /// Name a repository by its backend-relative identifier (for the native
    /// local backend, a path relative to the server's data directory).
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as a `&str`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RepoId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for RepoId {
    fn from(id: &str) -> Self {
        Self::new(id)
    }
}

/// A filter over [`Advertise::refs`](crate::Advertise::refs): which refs to
/// include. `prefix` follows [`git_backend::RefStore::iter_prefix`] — pass
/// `refs/` for a full advertisement (what `info/refs` needs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdSpec {
    /// Only refs at or under this prefix are advertised.
    pub prefix: RefName,
}

impl AdSpec {
    /// Advertise every ref (`refs/`).
    #[must_use]
    pub fn everything() -> Self {
        Self {
            prefix: RefName::new("refs/"),
        }
    }
}

/// The result of [`Advertise::refs`](crate::Advertise::refs): every ref
/// [`AdSpec`] selected, plus `HEAD`'s resolved tip when it points at one of
/// them (git's wire protocol advertises `HEAD` as a symref capability so a
/// client without an explicit branch in mind knows which one to check out).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RefAdvertisement {
    /// Every advertised ref and its current tip, in [`git_backend::RefStore::iter_prefix`]
    /// order.
    pub refs: Vec<(RefName, ObjectId)>,
    /// The ref `HEAD` currently resolves to, when it names one of `refs`
    /// (`None` if `HEAD` is unborn or points somewhere `AdSpec` excluded).
    pub head: Option<RefName>,
}

/// One round of want/have negotiation, handed to
/// [`Negotiate::wants_haves`](crate::Negotiate::wants_haves). A single round
/// is enough for the native backend's current (non-multi-round) negotiation;
/// the field is `&mut` so a future multi-round negotiator (stateless-RPC's
/// repeated flush packets) can extend this in place without changing the
/// trait signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NegotiationState {
    /// The repository being negotiated over.
    pub repo: RepoId,
    /// Object ids the client wants in the resulting pack.
    pub wants: Vec<ObjectId>,
    /// Object ids the client claims to already have — the pack must not
    /// resend anything reachable from these.
    pub haves: Vec<ObjectId>,
}

/// The result of [`Negotiate::wants_haves`](crate::Negotiate::wants_haves):
/// the exact object set [`GeneratePack::stream`](crate::GeneratePack::stream)
/// must pack, already reduced by the haves' reachable closure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackPlan {
    /// The repository the objects are read from.
    pub repo: RepoId,
    /// Objects to send, reachable from `wants` and not from `haves`.
    pub objects: Vec<ObjectId>,
}

/// A push, the way [`IngestPack::receive`](crate::IngestPack::receive)
/// receives it: the ref edits it asks for, the pack backing any new objects
/// they need, and the client's push certificate — required everywhere per
/// the uniform-strong attestation policy (`docs/scale-out.adoc`, "Attested
/// push"), except during a repository's bootstrap window (no members
/// enrolled yet).
pub struct PushRequest {
    /// The repository being pushed to.
    pub repo: RepoId,
    /// The ref updates this push asks for, applied as one atomic
    /// [`git_backend::RefStore::transaction`].
    pub ref_edits: Vec<RefEdit>,
    /// The pack carrying any objects the ref edits' new tips need that the
    /// repository doesn't already have. May be empty (a pure ref deletion
    /// still needs a valid, empty pack).
    pub pack: PackStream,
    /// The client-signed push certificate, verified before anything is
    /// staged. `None` is only accepted during the bootstrap window.
    pub push_cert: Option<PushCertificate>,
}

/// The client's signed push certificate, in the format `git push --signed`
/// produces: signed payload followed by an SSH signature block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushCertificate {
    /// The certificate's raw text, exactly as the client signed it.
    pub raw: String,
}

impl PushCertificate {
    /// Wrap `raw` certificate text.
    #[must_use]
    pub fn new(raw: impl Into<String>) -> Self {
        Self { raw: raw.into() }
    }
}

/// One ref edit as it actually applied — the *outcome* half of an
/// [`crate::attestation::OpRecord`], recorded alongside the client's *intent*
/// (its push certificate).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedRefEdit {
    /// The ref that changed.
    pub name: RefName,
    /// Its value before the push, or `None` if the ref did not exist.
    pub old: Option<ObjectId>,
    /// Its value after the push, or `None` if the push deleted it.
    pub new: Option<ObjectId>,
}

/// The result of [`IngestPack::receive`](crate::IngestPack::receive).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushOutcome {
    /// The push's ref transaction committed. `push_id` is the server-signed
    /// op record's own object id — per `docs/scale-out.adoc`, "Push ID = op
    /// record OID, uniformly."
    Accepted {
        /// The op record's object id, i.e. this push's id.
        push_id: ObjectId,
        /// The ref edits as applied.
        applied: Vec<AppliedRefEdit>,
    },
    /// The push was refused before anything was committed: a failed
    /// attestation check, a failed connectivity check, or a rejected
    /// compare-and-swap. No object staged for a rejected push is ever
    /// promoted or made reachable.
    Rejected {
        /// Why the push was refused.
        reason: String,
    },
}
