//! The Redaction entity: a record of a yank, not the yanked content.
//!
//! Spec coverage: `model.redaction`.

use ents_attrs as ents;
use facet::Facet;
use gix_hash::ObjectId;

/// A record that a specific object was redacted, living at
/// `refs/meta/redactions/<id>` (`namespace::redaction_ref`).
///
/// `model.redaction` requires the target's oid and a human-readable
/// reason, and forbids carrying the redacted content itself — this struct
/// has no field that could. It also carries no signature field: "the admin
/// signature authorizing the yank" is the enclosing mutation commit's own
/// signature (`receive.redaction-admin-only` is enforced there, by
/// `ents-receive`, phase 4), the same commit-chain-not-tree-field pattern
/// `model.comment` and `model.member-revocation` already follow.
///
/// `target` is stored as a raw 20-byte SHA-1 array — the one primitive
/// `facet-git-tree`'s byte-sequence encoding supports directly
/// (`gix_hash::ObjectId` itself has no `Facet` impl) — the same
/// representation `facet_git_tree::RawTree` uses internally for its own
/// wrapped oid. [`Redaction::new`] and [`Redaction::target`] keep the
/// public API in gitoxide's own type.
///
/// # Examples
///
/// ```
/// use ents_model::Redaction;
///
/// let target = gix_hash::ObjectId::null(gix_hash::Kind::Sha1);
/// let redaction = Redaction::new(target, "leaked credential");
/// assert_eq!(redaction.target(), target);
///
/// let (id, store) = facet_git_tree::serialize(&redaction).expect("serialize");
/// let back: Redaction = facet_git_tree::deserialize(&id, &store).expect("deserialize");
/// assert_eq!(back, redaction);
/// ```
// @relation(model.redaction, meta-ref.typed-tree, model.extensibility, scope=file)
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct Redaction {
    #[facet(ents::skip)]
    target: [u8; 20],
    /// A human-readable reason for the redaction.
    pub reason: String,
}

impl Redaction {
    /// Record that `target` was redacted for `reason`.
    #[must_use]
    pub fn new(target: ObjectId, reason: impl Into<String>) -> Self {
        let mut bytes = [0u8; 20];
        bytes.copy_from_slice(target.as_slice());
        Self {
            target: bytes,
            reason: reason.into(),
        }
    }

    /// The redacted object's id.
    #[must_use]
    pub fn target(&self) -> ObjectId {
        ObjectId::from_bytes_or_panic(&self.target)
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        clippy::panic,
        reason = "unit test; the panic is an assertion the type reflects as a struct at all"
    )]

    use facet::{Facet as _, Type, UserType};
    use facet_git_tree::{deserialize, serialize};
    use rstest::rstest;

    use super::*;

    #[rstest]
    // @relation(model.redaction, meta-ref.typed-tree, scope=function, role=Verifies)
    fn redaction_round_trips_and_preserves_the_target_oid() {
        let target = ObjectId::from_bytes_or_panic(&[7u8; 20]);
        let redaction = Redaction::new(target, "leaked credential");

        let (id, store) = serialize(&redaction).expect("serialize");
        let back: Redaction = deserialize(&id, &store).expect("deserialize");

        assert_eq!(back, redaction);
        assert_eq!(back.target(), target);
    }

    #[rstest]
    // @relation(model.redaction, scope=function, role=Verifies)
    fn redaction_never_carries_the_redacted_content() {
        let Type::User(UserType::Struct(struct_ty)) = Redaction::SHAPE.ty else {
            panic!("Redaction must reflect as a struct");
        };
        let names: Vec<_> = struct_ty.fields.iter().map(|f| f.name).collect();
        assert_eq!(
            names,
            vec!["target", "reason"],
            "Redaction must carry only the target oid and a reason, never the content itself"
        );
    }
}
