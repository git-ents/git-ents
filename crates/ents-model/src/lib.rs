//! The forge's entity vocabulary: structs, refname namespaces, reserved
//! commit trailers, and the one closed status taxonomy, all built directly
//! on `facet-git-tree`'s struct-to-tree mapping.
//!
//! Every other library crate in `git-ents` eventually imports this one
//! (`docs/spec/overview.sdoc`'s crate graph): `ents-query`, `ents-gate`,
//! `ents-anchor`, `ents-sync`, and `ents-web` all depend on `ents-model`
//! directly, and nothing here depends back on any of them. That is a
//! deliberate constraint, not an oversight — see [`Effect::trigger`] and
//! [`Comment::anchor`] for the two places a richer type would
//! have been the natural choice and was rejected specifically to keep this
//! edge one-directional.
//!
//! This crate is declarative on purpose: it defines *what* forge state
//! means (entity structs, taxonomy, namespace, trailers), never *how* it is
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
//! - `model.comment` — [`Comment`].
//! - `model.issue` — [`Issue`].
//! - `model.effect-definition` — [`Effect`].
//! - `model.result-taxonomy` — [`Status`].
//! - `model.toolchain` — [`Toolchain`].
//! - `model.redaction` — [`Redaction`].
//! - `model.account` — [`Account`].
//! - `meta-ref.namespace`, `meta-ref.granularity` — [`namespace`].
//! - `meta-ref.inbox` — [`namespace`], **partially**: the
//!   `refs/meta/inbox/*` half is implemented and tested
//!   ([`namespace::inbox_ref`], [`namespace::is_inbox`]). The
//!   `refs/meta/results/~<member>/...` results-mirror half is a spec rule
//!   that cannot be implemented as written — `~` is a byte
//!   `git-check-ref-format` (and `gix_validate::reference::name`, which
//!   mirrors it) rejects unconditionally in any refname, so no
//!   `gix::refs::FullName` can ever hold that shape. See the note above
//!   `namespace::inbox_ref` for the full detail; this is flagged as a STOP
//!   CONDITION rather than worked around with a substitute character.
//! - `meta-ref.typed-tree` — every entity module's round-trip test.
//! - `meta-ref.trailers` — [`trailer`].
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
//! [`Member`], place it under its namespace ref, bind a mutation commit to
//! that ref with a reserved trailer, and round-trip the entity through a
//! tree.
//!
//! ```
//! use ents_model::{Member, MemberId, Provenance, namespace, trailer::Trailers};
//!
//! let id = MemberId::new("jdc");
//! let member = Member::new("ssh-ed25519 AAAA... jdc", Provenance::AdminRegistered);
//!
//! // Where this member's ref lives.
//! let refname = namespace::member_ref(&id).expect("valid id");
//! assert_eq!(refname.as_bstr(), "refs/meta/member/jdc");
//!
//! // The commit that would write it binds itself to that ref via the
//! // reserved `Ents-Ref:` trailer (`meta-ref.trailers`).
//! let trailers = Trailers {
//!     ents_ref: Some(refname),
//!     schema_version: None,
//! };
//! let message = format!("Enroll jdc\n\n{}", trailers.render());
//! assert_eq!(Trailers::parse(message.as_bytes()), trailers);
//!
//! // The entity itself round-trips through `facet-git-tree` unchanged —
//! // the struct is the schema (`meta-ref.typed-tree`).
//! let (root, store) = facet_git_tree::serialize(&member).expect("serialize");
//! let back: Member = facet_git_tree::deserialize(&root, &store).expect("deserialize");
//! assert_eq!(back, member);
//! ```

mod account;
mod comment;
mod effect;
mod error;
mod issue;
mod member;
pub mod namespace;
mod redaction;
mod result;
mod toolchain;
pub mod trailer;

pub use account::Account;
pub use comment::Comment;
pub use effect::Effect;
pub use error::{Error, Result};
pub use issue::Issue;
pub use member::{Member, MemberId, MemberState, Provenance};
pub use redaction::Redaction;
pub use result::Status;
pub use toolchain::Toolchain;

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
    #[case::comment(Comment::SHAPE.type_identifier, "Comment")]
    #[case::effect(Effect::SHAPE.type_identifier, "Effect")]
    #[case::issue(Issue::SHAPE.type_identifier, "Issue")]
    #[case::member(Member::SHAPE.type_identifier, "Member")]
    #[case::redaction(Redaction::SHAPE.type_identifier, "Redaction")]
    #[case::status(Status::SHAPE.type_identifier, "Status")]
    #[case::toolchain(Toolchain::SHAPE.type_identifier, "Toolchain")]
    // @relation(model.extensibility, scope=function, role=Verifies)
    fn every_entity_shape_name_tracks_its_struct_declaration(
        #[case] reflected: &str,
        #[case] expected: &str,
    ) {
        assert_eq!(reflected, expected);
    }
}
