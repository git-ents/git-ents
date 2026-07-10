//! The Comment entity: a body anchored to specific content.
//!
//! Spec coverage: `model.comment`.

use facet::Facet;
use facet_git_tree::RawTree;

/// A body of text anchored to the exact content it was written against.
///
/// `model.comment` requires a body and an anchor, and that a comment's
/// author and timestamp come from the mutation commit chain rather than a
/// stored field — the same rule `meta-ref.trailers` states for ref-level
/// metadata generally. `Comment` therefore has no author or timestamp
/// field.
///
/// The anchor itself is stored as an opaque [`RawTree`]: `anchor.adoc`
/// (`anchor.definition`, `anchor.retention`, `anchor.projection`) defines
/// what it identifies and how it survives force-push and gc, and is owned
/// by `ents-anchor` (phase 3 — not started by this crate). `ents-model`
/// only reserves the slot `model.comment` requires; the tree `ents-anchor`
/// writes there must already exist in the store being serialized into, per
/// `RawTree`'s own contract.
///
/// # Examples
///
/// ```
/// use ents_model::Comment;
/// use facet_git_tree::{ObjectStore, RawTree};
/// use gix_object::{Kind, Write as _};
///
/// // Stand in for what `ents-anchor` will actually write: any pre-existing
/// // tree, embedded unchanged.
/// let store = ObjectStore::default();
/// let anchor_tree = gix_object::Tree { entries: vec![] };
/// let anchor_oid = store.write(&anchor_tree).expect("tree");
///
/// let comment = Comment {
///     body: "this line looks off by one".to_owned(),
///     anchor: RawTree::new(anchor_oid),
/// };
/// let root = facet_git_tree::serialize_into(&comment, &store).expect("serialize");
/// let back: Comment = facet_git_tree::deserialize(&root, &store).expect("deserialize");
/// assert_eq!(back, comment);
/// ```
// @relation(model.comment, meta-ref.typed-tree, model.extensibility, scope=file)
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct Comment {
    /// The comment's text.
    pub body: String,
    /// The anchor identifying the exact content the comment was written
    /// against (`anchor.definition`), opaque to this crate.
    pub anchor: RawTree,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "unit test")]

    use facet_git_tree::{ObjectStore, deserialize, serialize_into};
    use gix_object::Write as _;
    use rstest::rstest;

    use super::*;

    #[rstest]
    // @relation(model.comment, meta-ref.typed-tree, scope=function, role=Verifies)
    fn comment_round_trips_through_a_tree() {
        let store = ObjectStore::default();
        let anchor_tree = gix_object::Tree { entries: vec![] };
        let anchor_oid = store.write(&anchor_tree).expect("tree");

        let comment = Comment {
            body: "looks off by one".to_owned(),
            anchor: RawTree::new(anchor_oid),
        };
        let root = serialize_into(&comment, &store).expect("serialize");
        let back: Comment = deserialize(&root, &store).expect("deserialize");
        assert_eq!(back, comment);
    }
}
