//! The forge domain: the [`Issue`], [`comment::Comment`], and
//! [`review::Review`] entities, and the command business logic driving
//! each — kernel-independent, unlike `ents-model`'s remaining entities,
//! because a comment or review command needs `ents-anchor` (to capture and
//! project a code anchor) and `ents-receive` (to propose the mutation),
//! neither of which a purely declarative vocabulary crate like
//! `ents-model` may depend on.
//!
//! This crate sits *above* the kernel in the dependency graph, not inside
//! it: `ents-model`, `ents-anchor`, `ents-gate`, `ents-query`,
//! `ents-receive`, `ents-effect`, `ents-sync`, and `ents-testutil` must
//! never depend on `ents-forge` (verified by `grep -rn ents-forge
//! crates/kernel crates/substrate` finding nothing) — `ents-forge` depends
//! on them, never the reverse. `git-ents` (the CLI) depends on this crate
//! and mounts each command through a thin wrapper that only adds
//! signer/actor construction and CLI-facing error rendering
//! (`crate::mutate::outcome_to_result` on the CLI side).
//!
//! # Spec coverage
//!
//! From `docs/spec/model.adoc` and `docs/spec/meta-ref.adoc`:
//!
//! - `model.issue` — [`Issue`] and the command layer around it
//!   ([`issue::new`], [`issue::edit`], [`issue::list`], [`issue::show`]).
//! - `model.comment`, `model.comment-state`, `model.comment-context`,
//!   `model.comment-thread` — [`comment::Comment`] and the command layer
//!   around it ([`comment::add`], [`comment::reply`],
//!   [`comment::resolve`]/[`comment::reopen`], [`comment::thread`]).
//! - `model.review`, `model.review-pin` — [`review::Review`] and
//!   [`review::new`] (writes both the entity ref and the retention pin),
//!   [`review::list`], [`review::show`] (reusing [`comment::thread`] for
//!   the review's discussion rather than a second aggregation).
//! - `meta-ref.granularity` — one ref per issue/comment/review
//!   (`refs/meta/issues/<id>`, `refs/meta/comments/<id>`,
//!   `refs/meta/reviews/<id>`); see [`comment::add`] and [`review::new`]
//!   for how an id is generated locally rather than derived from the
//!   entity itself.
//! - `meta-ref.typed-tree` — every entity module's round-trip test.
//! - `anchor.definition`, `anchor.projection`, `anchor.working-tree` —
//!   [`comment::add`], [`comment::show`], and [`comment::list_projected`],
//!   built directly on `ents_anchor::capture`/`capture_worktree` and
//!   `project`/`project_worktree`.
//! - `lens.parity` — every operation the CLI, the web UI, or an editor
//!   lens offers over these entities is one of this crate's library
//!   functions; frontends only wire stores and render.
//!
//! # Examples
//!
//! Build an [`Issue`], and a [`comment::Comment`] anchored to a stand-in
//! tree (`ents-anchor` owns capturing a real anchor from a repository;
//! this crate only defines the entity slot and the command driving it) —
//! both round-trip through `facet-git-tree` unchanged, the
//! schema-is-the-struct property `meta-ref.typed-tree` requires.
//!
//! ```
//! use ents_forge::Issue;
//! use ents_forge::comment::Comment;
//! use ents_model::MemberId;
//! use facet_git_tree::RawTree;
//! use gix_object::Write as _;
//!
//! let issue = Issue {
//!     title: "gate rejects a valid signature".to_owned(),
//!     body: "steps to reproduce...".to_owned(),
//!     state: "triaged".to_owned(),
//!     assignees: vec![MemberId::new("jdc")],
//!     labels: vec!["bug".to_owned()],
//! };
//! let (id, store) = facet_git_tree::serialize(&issue).expect("serialize");
//! let back: Issue = facet_git_tree::deserialize(&id, &store).expect("deserialize");
//! assert_eq!(back, issue);
//!
//! let store = facet_git_tree::ObjectStore::default();
//! let anchor_tree = store.write(&gix_object::Tree { entries: vec![] }).expect("tree");
//! let comment = Comment {
//!     body: "looks off by one".to_owned(),
//!     state: "open".to_owned(),
//!     anchor: Some(RawTree::new(anchor_tree)),
//!     context: Some("issues/42".to_owned()),
//!     parent: None,
//! };
//! let root = facet_git_tree::serialize_into(&comment, &store).expect("serialize");
//! let back: Comment = facet_git_tree::deserialize(&root, &store).expect("deserialize");
//! assert_eq!(back, comment);
//! ```

mod error;

pub mod comment;
pub mod issue;
pub mod review;

pub use error::{Error, Result};
pub use issue::Issue;

#[cfg(test)]
mod tests {
    use facet::Facet as _;
    use rstest::rstest;

    use super::*;

    /// Every entity this crate owns keeps the same `model.extensibility`
    /// guarantee `ents_model`'s own shape test pins for its remaining
    /// entities: each type's reflected [`facet::Shape::type_identifier`]
    /// is exactly its Rust struct name.
    #[rstest]
    #[case::comment(comment::Comment::SHAPE.type_identifier, "Comment")]
    #[case::issue(Issue::SHAPE.type_identifier, "Issue")]
    #[case::review(review::Review::SHAPE.type_identifier, "Review")]
    // @relation(model.extensibility, scope=function, role=Verifies)
    fn every_entity_shape_name_tracks_its_struct_declaration(
        #[case] reflected: &str,
        #[case] expected: &str,
    ) {
        assert_eq!(reflected, expected);
    }
}
