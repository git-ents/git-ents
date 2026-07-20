//! The Claim entity: a signer's verdict on an [`ents_anchor::Binding`],
//! under an opaque kind — the kernel's shared building block for a
//! comment's thread state, a review's approval, a CI result, or any other
//! package's assertion about the object graph, without the kernel ever
//! enumerating what those assertions mean.
//!
//! No spec id is assigned to this entity yet — claim vocabularies are
//! being specified separately — so nothing in this module carries an
//! `@relation` marker.

use facet::Facet;
use facet_git_tree::RawTree;
use gix_object::{Find, Write};

use crate::error::{Error, Result};
use crate::member::MemberId;

/// A claim's verdict: what its signer asserts about its binding.
///
/// Parses from and renders as its kebab-case convention names (`affirm`,
/// `deny`, `note`), the same strings every surface shows — modeled on
/// `ents_forge::review::Verdict`.
///
/// # Examples
///
/// ```
/// use ents_model::claim::Verdict;
///
/// let verdict: Verdict = "deny".parse().expect("known verdict");
/// assert_eq!(verdict, Verdict::Deny);
/// assert_eq!(verdict.to_string(), "deny");
/// assert!("maybe".parse::<Verdict>().is_err());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Facet)]
#[repr(u8)]
pub enum Verdict {
    /// The claim affirms its binding.
    Affirm,
    /// The claim denies its binding.
    Deny,
    /// Judgment withheld: the claim exists for its kind and context alone.
    Note,
}

impl std::str::FromStr for Verdict {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self> {
        match text {
            "affirm" => Ok(Self::Affirm),
            "deny" => Ok(Self::Deny),
            "note" => Ok(Self::Note),
            other => Err(Error::InvalidArgument(format!(
                "unknown verdict {other:?}: expected affirm, deny, or note"
            ))),
        }
    }
}

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Affirm => "affirm",
            Self::Deny => "deny",
            Self::Note => "note",
        })
    }
}

/// A signer's verdict on a binding, under an opaque kind.
///
/// A `Claim` lives one-ref-per-claim at `refs/meta/claims/<id>`
/// ([`crate::namespace::claim_ref`]), where `<id>` is the claim's own
/// genesis commit oid: the repository's standard sign-then-name envelope,
/// exactly like a comment or an issue. Unlike those, a claim ref is
/// append-once — the tip IS the genesis, and a changed assertion is a new
/// claim, never an advance — so `signer` here must equal the ledger
/// commit's actual signer, which the gate binds
/// (`meta-ref.identity-binding`'s natural-key shape, applied to a
/// genesis-only ref).
///
/// `binding` embeds an [`ents_anchor::Binding`] by tree id, opaque to this
/// crate exactly as `ents-forge`'s `Comment::anchor` embeds an anchor —
/// [`Claim::new`] and [`Claim::binding`] do the round trip so no caller
/// hand-wires the tree. The claim's ledger commit carries the
/// binding's own [`ents_anchor::Binding::witnesses`] as its parents, which
/// is what keeps the bound objects reachable; building that commit is a
/// caller concern (`ents_receive`), not this struct's.
///
/// `kind` is a plain opaque string: kind vocabularies (`review`, `ci`, or
/// anything else a package invents) are package policy, never kernel
/// enumeration — this crate never matches on it.
///
/// # Examples
///
/// ```
/// use ents_anchor::Binding;
/// use ents_model::MemberId;
/// use ents_model::claim::{Claim, Verdict};
/// use facet_git_tree::ObjectStore;
///
/// let store = ObjectStore::default();
/// let commit = gix::ObjectId::from_hex(b"0123456789abcdef0123456789abcdef01234567")
///     .expect("valid hex");
/// let binding = Binding::Commit { commit };
///
/// let claim = Claim::new(MemberId::new("jdc"), &binding, Verdict::Affirm, "review", &store)
///     .expect("serialize");
/// assert_eq!(claim.kind, "review");
///
/// // The binding round-trips through the claim unchanged.
/// let back = claim.binding(&store).expect("deserialize");
/// assert_eq!(back, binding);
///
/// // The claim itself is an ordinary typed tree.
/// let root = facet_git_tree::serialize_into(&claim, &store).expect("serialize claim");
/// let claim_back: Claim = facet_git_tree::deserialize(&root, &store).expect("deserialize claim");
/// assert_eq!(claim_back, claim);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct Claim {
    /// The member who signed this claim — must equal the ledger commit's
    /// actual signer, which the gate binds.
    pub signer: MemberId,
    /// The serialized [`ents_anchor::Binding`]'s tree, embedded by tree id
    /// ([`Claim::new`], [`Claim::binding`] do the round trip).
    pub binding: RawTree,
    /// What the signer asserts about the binding.
    pub verdict: Verdict,
    /// An opaque kind string — package policy, never kernel vocabulary.
    pub kind: String,
}

impl Claim {
    /// Build a claim of `binding`, serializing it into `store` and
    /// recording the resulting tree by id.
    ///
    /// # Errors
    ///
    /// [`Error::Anchor`] if `binding` cannot be serialized into `store`.
    pub fn new<W: Write + ?Sized>(
        signer: MemberId,
        binding: &ents_anchor::Binding,
        verdict: Verdict,
        kind: impl Into<String>,
        store: &W,
    ) -> Result<Self> {
        let root = binding.serialize_into(store)?;
        Ok(Self {
            signer,
            binding: RawTree::new(root),
            verdict,
            kind: kind.into(),
        })
    }

    /// Read this claim's binding back out of `store`.
    ///
    /// # Errors
    ///
    /// [`Error::Anchor`] if the recorded tree cannot be read or does not
    /// match any known [`ents_anchor::Binding`] shape.
    pub fn binding<F: Find + ?Sized>(&self, store: &F) -> Result<ents_anchor::Binding> {
        Ok(ents_anchor::Binding::deserialize(
            &self.binding.oid(),
            store,
        )?)
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        reason = "unit test"
    )]

    use ents_anchor::Binding;
    use facet::{Facet as _, Type, UserType};
    use facet_git_tree::{ObjectStore, deserialize, serialize_into};
    use gix_hash::ObjectId;
    use rstest::rstest;

    use super::*;

    fn hex(byte: u8) -> ObjectId {
        let hex_digit = format!("{byte:x}");
        let full = hex_digit.repeat(40);
        ObjectId::from_hex(full.as_bytes()).unwrap()
    }

    #[rstest]
    #[case::affirm(Verdict::Affirm)]
    #[case::deny(Verdict::Deny)]
    #[case::note(Verdict::Note)]
    fn claim_round_trips_with_every_verdict(#[case] verdict: Verdict) {
        let store = ObjectStore::default();
        let binding = Binding::Commit { commit: hex(1) };
        let claim = Claim::new(MemberId::new("jdc"), &binding, verdict, "review", &store)
            .expect("new claim");

        let root = serialize_into(&claim, &store).expect("serialize");
        let back: Claim = deserialize(&root, &store).expect("deserialize");
        assert_eq!(back, claim);
    }

    #[rstest]
    #[case::commit(Binding::Commit { commit: hex(1) })]
    #[case::tree(Binding::Tree {
        tree: hex(2),
        path: "src/lib.rs".to_owned(),
        witness: hex(3),
    })]
    fn new_and_binding_round_trip_an_actual_binding(#[case] binding: Binding) {
        let store = ObjectStore::default();
        let claim = Claim::new(
            MemberId::new("jdc"),
            &binding,
            Verdict::Affirm,
            "review",
            &store,
        )
        .expect("new claim");
        let back = claim.binding(&store).expect("read binding back");
        assert_eq!(back, binding);
    }

    #[test]
    fn field_order_is_signer_binding_verdict_kind() {
        let Type::User(UserType::Struct(struct_ty)) = Claim::SHAPE.ty else {
            panic!("Claim must reflect as a struct");
        };
        let names: Vec<_> = struct_ty.fields.iter().map(|f| f.name).collect();
        assert_eq!(names, vec!["signer", "binding", "verdict", "kind"]);
    }
}
