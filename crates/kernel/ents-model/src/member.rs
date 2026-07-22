//! The Member entity: an enrolled public key, and the trust state it carries.
//!
//! Spec coverage: `model.member-identity`, `model.member-revocation`,
//! `model.member-provenance`, `model.member-worker`.

use facet::Facet;

/// The stable id naming one member's ref, `refs/meta/member/<id>`
/// (`namespace::member_ref`).
///
/// A newtype rather than a bare `String` because a member id is forge
/// vocabulary gitoxide has no concept of — unlike a refname or object id, it
/// is not a git primitive, so wrapping it here does not duplicate one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Facet)]
#[facet(transparent)]
pub struct MemberId(pub String);

impl MemberId {
    /// Build a member id from any string-like value.
    ///
    /// # Examples
    ///
    /// ```
    /// use ents_model::MemberId;
    ///
    /// let id = MemberId::new("jdc");
    /// assert_eq!(id.as_str(), "jdc");
    /// ```
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Borrow the id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for MemberId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for MemberId {
    fn from(id: String) -> Self {
        Self(id)
    }
}

impl From<&str> for MemberId {
    fn from(id: &str) -> Self {
        Self(id.to_owned())
    }
}

impl From<&MemberId> for MemberId {
    fn from(id: &MemberId) -> Self {
        id.clone()
    }
}

/// Whether a member's key currently authorizes new signatures.
///
/// `model.member-revocation` requires that revoking a member record a state
/// on the entity rather than delete it, and that a signature made before
/// revocation remain verifiable while one made after is rejected. That
/// before/after judgment is made by walking the member ref's own commit
/// history (`meta-ref.namespace`: the commit chain is the audit trail) for
/// the state in force at the signature's time, exactly as a comment's
/// author and timestamp come from the mutation commit rather than a stored
/// field (`model.comment`) — so this type only needs to carry the *current*
/// state, never a validity window.
// @relation(model.member-revocation, scope=file)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Facet)]
#[repr(u8)]
pub enum MemberState {
    /// The key authorizes new signatures.
    Active,
    /// The key does not authorize new signatures made after the commit that
    /// set this state; signatures it made earlier remain verifiable.
    Revoked,
}

impl std::fmt::Display for MemberState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Active => "active",
            Self::Revoked => "revoked",
        })
    }
}

/// How a member came to be enrolled.
///
/// `model.member-provenance` ties authorization for canonical refs to this
/// field: a self-attested member is limited to its own inbox and self-run
/// namespaces (`meta-ref.inbox`) until an admin-registered member promotes
/// it by an ordinary signed mutation of the member's ref. Enforcing that
/// restriction is `ents-gate`'s job (`effect.admin-only`); this type only
/// records which case applies.
// @relation(model.member-provenance, scope=file)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Facet)]
#[repr(u8)]
pub enum Provenance {
    /// Enrolled by an admin-registered member mutating the new member's
    /// ref directly.
    AdminRegistered,
    /// Self-attested through a frontend that lets a key enroll itself.
    SelfAttested,
}

/// A public key enrolled into the forge's trust set.
///
/// `model.member-identity` requires a `Member` to carry both the key
/// itself and its member id — the id being the natural key the refname
/// `refs/meta/member/<id>` (`namespace::member_ref`) binds to
/// (`meta-ref.identity-binding`): the gate recomputes the refname's final
/// segment from this tree field and refuses a mismatch, so the id is a
/// total function of signed content, not a refname the tree merely trusts.
/// Enrollment is the signed commit that writes the entity, not a field on
/// the struct.
///
/// `model.member-worker` requires that a machine actor (a CI worker or
/// other automated signer) be an ordinary `Member` with no privileged
/// construction path. This type has exactly one constructor
/// ([`Member::new`]) for both cases; nothing here distinguishes a human
/// key from a machine key beyond the [`Provenance`] every member already
/// carries.
///
/// # Examples
///
/// ```
/// use ents_model::{Member, MemberState, Provenance};
///
/// // A human member, admin-registered.
/// let human = Member::new("joey", "ssh-ed25519 AAAA... joey", Provenance::AdminRegistered);
/// assert_eq!(human.state, MemberState::Active);
///
/// // A CI worker's key is enrolled through the exact same constructor —
/// // `model.member-worker` forbids a separate privileged path.
/// let worker = Member::new("ci-worker", "ssh-ed25519 AAAA... ci-worker", Provenance::AdminRegistered);
/// assert_eq!(worker.provenance, Provenance::AdminRegistered);
/// ```
// @relation(model.member-identity, model.member-worker, meta-ref.identity-binding, meta-ref.typed-tree, model.extensibility, scope=file)
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct Member {
    /// The member's id — the natural key the refname's final segment binds
    /// to (`model.member-identity`, `meta-ref.identity-binding`).
    pub id: MemberId,
    /// The member's public key material, in whatever text form the
    /// deployment's signature verification expects (an OpenSSH public key
    /// line, an armored PGP key, etc.). `ents-gate` (phase 3) interprets
    /// this; `ents-model` treats it as opaque.
    pub key: String,
    /// Whether the key currently authorizes new signatures.
    pub state: MemberState,
    /// How the member was enrolled.
    pub provenance: Provenance,
}

impl Member {
    /// Enroll a new member, active from the start.
    ///
    /// This is the sole constructor — used identically for a human member
    /// and a machine actor (`model.member-worker`). `id` MUST equal the
    /// final segment of the member's refname, the binding the gate
    /// recomputes (`meta-ref.identity-binding`).
    #[must_use]
    pub fn new(id: impl Into<MemberId>, key: impl Into<String>, provenance: Provenance) -> Self {
        Self {
            id: id.into(),
            key: key.into(),
            state: MemberState::Active,
            provenance,
        }
    }

    /// Record a revoked state without deleting the entity
    /// (`model.member-revocation`).
    ///
    /// # Examples
    ///
    /// ```
    /// use ents_model::{Member, MemberState, Provenance};
    ///
    /// let mut member = Member::new("jdc", "key", Provenance::AdminRegistered);
    /// member.revoke();
    /// assert_eq!(member.state, MemberState::Revoked);
    /// ```
    pub fn revoke(&mut self) {
        self.state = MemberState::Revoked;
    }

    /// Return the key to authorizing new signatures
    /// (`model.member-revocation`'s unrevoke case). The record of the
    /// revoked period itself lives in the ref's commit history, not in this
    /// struct, so unrevoking alters only the current state.
    ///
    /// # Examples
    ///
    /// ```
    /// use ents_model::{Member, MemberState, Provenance};
    ///
    /// let mut member = Member::new("jdc", "key", Provenance::AdminRegistered);
    /// member.revoke();
    /// member.unrevoke();
    /// assert_eq!(member.state, MemberState::Active);
    /// ```
    pub fn unrevoke(&mut self) {
        self.state = MemberState::Active;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "unit test")]

    use facet_git_tree::{deserialize, serialize};
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::admin_human(Provenance::AdminRegistered)]
    #[case::self_attested_human(Provenance::SelfAttested)]
    // @relation(model.member-worker, model.member-provenance, scope=function, role=Verifies)
    fn worker_and_human_share_one_constructor(#[case] provenance: Provenance) {
        // A "worker" is not a distinct type or constructor — just another
        // key enrolled the same way, which this parameterization over the
        // same `Member::new` demonstrates directly.
        let worker = Member::new("ci-worker", "ssh-ed25519 AAAA... ci-worker", provenance);
        let human = Member::new("joey", "ssh-ed25519 AAAA... joey", provenance);
        assert_eq!(worker.provenance, human.provenance);
    }

    #[rstest]
    // @relation(model.member-revocation, scope=function, role=Verifies)
    fn revoke_then_unrevoke_round_trips_to_active() {
        let mut member = Member::new("jdc", "key", Provenance::AdminRegistered);
        assert_eq!(member.state, MemberState::Active);
        member.revoke();
        assert_eq!(member.state, MemberState::Revoked);
        member.unrevoke();
        assert_eq!(member.state, MemberState::Active);
    }

    #[rstest]
    #[case::active(MemberState::Active, "active")]
    #[case::revoked(MemberState::Revoked, "revoked")]
    // @relation(model.member-revocation, scope=function, role=Verifies)
    fn member_state_displays_lowercase(#[case] state: MemberState, #[case] expected: &str) {
        assert_eq!(state.to_string(), expected);
    }

    #[rstest]
    #[case::active(MemberState::Active)]
    #[case::revoked(MemberState::Revoked)]
    // @relation(model.member-identity, meta-ref.typed-tree, scope=function, role=Verifies)
    fn member_round_trips_through_a_tree(#[case] state: MemberState) {
        let member = Member {
            id: MemberId::new("jdc"),
            key: "ssh-ed25519 AAAA... jdc".to_owned(),
            state,
            provenance: Provenance::AdminRegistered,
        };
        let (id, store) = serialize(&member).expect("serialize");
        let back: Member = deserialize(&id, &store).expect("deserialize");
        assert_eq!(member, back);
    }
}
