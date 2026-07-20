//! The forge's entity vocabulary: structs, refname namespaces, and the one
//! closed status taxonomy, all built directly on `facet-git-tree`'s
//! struct-to-tree mapping.
//!
//! Every other library crate in `git-ents` eventually imports this one
//! (`docs/spec/overview.sdoc`'s crate graph): `ents-query`, `ents-gate`,
//! `ents-anchor`, `ents-sync`, and `ents-web` all depend on `ents-model`
//! directly, and nothing here depends back on any of them. That is a
//! deliberate constraint, not an oversight — see [`Effect::trigger`] and
//! `Comment::anchor` (in `ents-forge`) for the two places a richer type would
//! have been the natural choice and was rejected specifically to keep this
//! edge one-directional.
//!
//! This crate is declarative on purpose: it defines *what* forge state
//! means (entity structs, taxonomy, namespace), never *how* it is
//! verified, queried, or executed. Those verbs belong to `ents-gate`,
//! `ents-query`, and `ents-effect` respectively (`docs/spec/overview.sdoc`,
//! "Boundary Rules").
//!
//! # Spec coverage
//!
//! This crate implements, from `docs/spec/model.sdoc` and
//! `docs/spec/meta-ref.sdoc`:
//!
//! - `model.extensibility` — every entity here is a compile-time
//!   `#[derive(Facet)]` struct; see the crate-level test that reflects each
//!   one's [`facet::Shape`] rather than relying on a runtime schema.
//! - `model.member-identity`, `model.member-revocation`,
//!   `model.member-provenance`, `model.member-worker` — [`Member`].
//! - `model.comment`, `model.issue` — moved to `ents-forge` (the forge
//!   domain needs `ents-anchor` and `ents-receive`, which a purely
//!   declarative vocabulary crate like this one may not depend on); see
//!   `ents-forge`'s `Issue` and `Comment`.
//! - `model.effect-definition` — [`Effect`].
//! - `model.result-taxonomy` — [`Status`].
//! - `model.result-identity` — [`ResultRecord`].
//! - `model.toolchain` — moved to `ents-kiln` (resolving and materializing
//!   a toolchain needs `ents-effect`'s toolchain-resolution machinery,
//!   which a purely declarative vocabulary crate like this one may not
//!   depend on); see `ents-kiln`'s `Toolchain`.
//! - `model.redaction` — [`Redaction`].
//! - `model.account` — [`Account`].
//!
//! [`Claim`] (`refs/meta/claims/*`, [`namespace::claim_ref`]) is also
//! defined here: a signer × binding × verdict × opaque-kind entity, the
//! shared building block a comment's thread state, a review's approval, or
//! a CI result can each be built from without the kernel enumerating what
//! any of them mean. No spec id covers it yet — see the [`claim`] module's
//! own doc comment.
//! - `meta-ref.namespace`, `meta-ref.granularity` — [`namespace`].
//! - `meta-ref.inbox` — [`namespace`]: the `refs/meta/inbox/<member>/<id>`
//!   half ([`namespace::inbox_ref`], [`namespace::inbox_owner`],
//!   [`namespace::is_inbox`]) and the
//!   `refs/meta/self/<member>/<effect>/<short-oid>` self-run mirror half
//!   ([`namespace::self_result_ref`], [`namespace::self_run_owner`]).
//! - `meta-ref.typed-tree` — every entity module's round-trip test.
//! - `meta-ref.identity-binding` — the natural-key tree fields
//!   ([`Member::id`], [`Effect::name`]) and composite key fields
//!   ([`ResultRecord::effect`], `ResultRecord::target`) the gate
//!   recomputes a refname from, plus the composite review/result refname
//!   builders and parsers in [`namespace`]; the recomputation itself is
//!   `ents-gate`'s (`gate.identity-binding`).
//!
//! Two `meta-ref.sdoc` rules are deliberately not implemented here:
//! `meta-ref.tip-invariant` (a non-owning reader degrading to opaque
//! display, and surfacing a redaction marker) needs a wired-up
//! `RefStoreRead` and object access, which belongs to a reading crate
//! (`ents-receive` or the `git-ents` binary, both later phases) — this
//! crate only defines the [`Redaction`] entity such a marker would
//! describe. `meta-ref.migration` is enacted by whichever crate performs a
//! write (`ents-receive`, phase 4: a signed commit on top of the old tip);
//! the one constraint that is this crate's to keep — no version-marker
//! entry in the tree — is `meta-ref.typed-tree`, already covered.
//!
//! # Examples
//!
//! A worked round trip through every layer this crate owns: build a
//! [`Member`], place it under its namespace ref whose final segment its id
//! field binds (`meta-ref.identity-binding`), and round-trip the entity
//! through a tree.
//!
//! ```
//! use ents_model::{Member, MemberId, Provenance, namespace};
//!
//! let id = MemberId::new("jdc");
//! let member = Member::new(&id, "ssh-ed25519 AAAA... jdc", Provenance::AdminRegistered);
//!
//! // Where this member's ref lives — its final segment is the id field the
//! // gate recomputes from the signed tree (`meta-ref.identity-binding`).
//! let refname = namespace::member_ref(&id).expect("valid id");
//! assert_eq!(refname.as_bstr(), "refs/meta/member/jdc");
//! assert_eq!(member.id, id);
//!
//! // The entity itself round-trips through `facet-git-tree` unchanged —
//! // the struct is the schema (`meta-ref.typed-tree`).
//! let (root, store) = facet_git_tree::serialize(&member).expect("serialize");
//! let back: Member = facet_git_tree::deserialize(&root, &store).expect("deserialize");
//! assert_eq!(back, member);
//! ```

mod account;
pub mod claim;
mod effect;
mod error;
mod member;
pub mod namespace;
mod redaction;
mod result;

pub use account::Account;
pub use claim::Claim;
pub use effect::Effect;
pub use error::{Error, Result};
pub use member::{Member, MemberId, MemberState, Provenance};
pub use redaction::Redaction;
pub use result::{ResultRecord, Status};

#[cfg(test)]
mod tests {
    use facet::Facet as _;
    use rstest::rstest;

    use super::*;

    /// `model.extensibility` requires an entity's shape to come from its
    /// `#[derive(Facet)]` struct at compile time, never from data read at
    /// runtime. This asserts the concrete, checkable half of that: each
    /// type's reflected [`facet::Shape::type_identifier`] is exactly its
    /// Rust struct name, so the shape tracks the source declaration
    /// automatically — extending an entity is only possible by changing
    /// the struct and recompiling, never by pointing the same struct at
    /// different runtime-supplied field data.
    #[rstest]
    #[case::account(Account::SHAPE.type_identifier, "Account")]
    #[case::claim(Claim::SHAPE.type_identifier, "Claim")]
    #[case::claim_verdict(claim::Verdict::SHAPE.type_identifier, "Verdict")]
    #[case::effect(Effect::SHAPE.type_identifier, "Effect")]
    #[case::member(Member::SHAPE.type_identifier, "Member")]
    #[case::redaction(Redaction::SHAPE.type_identifier, "Redaction")]
    #[case::result(ResultRecord::SHAPE.type_identifier, "ResultRecord")]
    #[case::status(Status::SHAPE.type_identifier, "Status")]
    // @relation(model.extensibility, scope=function, role=Verifies)
    fn every_entity_shape_name_tracks_its_struct_declaration(
        #[case] reflected: &str,
        #[case] expected: &str,
    ) {
        assert_eq!(reflected, expected);
    }
}
