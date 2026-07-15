//! The Comment entity: a body about something — an anchor, a context
//! entity, a parent comment, or any combination.
//!
//! Spec coverage: `model.comment`, `model.comment-state`,
//! `model.comment-context`, `model.comment-thread`.

use facet::Facet;
use facet_git_tree::RawTree;
use gix_hash::ObjectId;
use gix_object::Find;

/// A body of text about something: an anchor into content, a context
/// entity, a parent comment, or any combination (`model.comment`).
///
/// `model.comment` requires a body and that the comment identify what it
/// is about; a comment about nothing is refused at creation by the writing
/// tool ([`Comment::is_about_nothing`], enforced in [`super::add`]), never
/// by the gate, which stays content-agnostic. Author and timestamp come
/// from the mutation commit chain rather than a stored field
/// (`meta-ref.identity-binding`) — `Comment` therefore has no author or timestamp
/// field, and no reviewer/resolver field either: who changed [`Comment::state`],
/// and when, is the mutation chain's answer too (`model.comment-state`).
///
/// The anchor, when present, is stored as an opaque [`RawTree`]:
/// `anchor.adoc` (`anchor.definition`, `anchor.retention`,
/// `anchor.projection`, `anchor.working-tree`) defines what it identifies
/// and how it survives force-push and gc, and it is owned by
/// [`ents_anchor`], this crate's own dependency for anchoring a comment to
/// code (`super::command`).
///
/// # Examples
///
/// ```
/// use ents_forge::comment::Comment;
/// use facet_git_tree::{ObjectStore, RawTree};
/// use gix_object::{Kind, Write as _};
///
/// // Stand in for what `ents-anchor` actually writes: any pre-existing
/// // tree, embedded unchanged.
/// let store = ObjectStore::default();
/// let anchor_tree = gix_object::Tree { entries: vec![] };
/// let anchor_oid = store.write(&anchor_tree).expect("tree");
///
/// let comment = Comment {
///     body: "this line looks off by one".to_owned(),
///     state: "open".to_owned(),
///     anchor: Some(RawTree::new(anchor_oid)),
///     context: Some("issues/42".to_owned()),
///     parent: None,
/// };
/// let root = facet_git_tree::serialize_into(&comment, &store).expect("serialize");
/// let back: Comment = facet_git_tree::deserialize(&root, &store).expect("deserialize");
/// assert_eq!(back, comment);
/// ```
// @relation(model.comment, model.comment-state, model.comment-context, model.comment-thread, meta-ref.typed-tree, model.extensibility, scope=file)
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct Comment {
    /// The comment's text.
    pub body: String,
    /// The comment's state (`model.comment-state`): `open` for a new
    /// comment, `resolved` once resolved — not a fixed enum, because
    /// custom states are schema, not platform features, exactly as for
    /// issues (`model.issue`).
    pub state: String,
    /// The anchor identifying the exact content the comment was written
    /// against (`anchor.definition`), opaque to this crate; `None` for a
    /// comment about a context entity or a parent comment only.
    pub anchor: Option<RawTree>,
    /// The canonical ref path below `refs/meta/` of the entity this
    /// comment belongs to, such as `issues/<id>` or `reviews/<target>/<member>`
    /// (`model.comment-context`) — an entity's thread is an aggregation
    /// query over comments naming it, never a list the entity stores.
    pub context: Option<String>,
    /// The id of the comment this one replies to (`model.comment-thread`);
    /// a reply inherits its aboutness from its thread root rather than
    /// repeating an anchor or context.
    pub parent: Option<String>,
}

impl Comment {
    /// Whether this comment identifies nothing at all — no anchor, no
    /// context, no parent. `model.comment` requires the writing tool to
    /// refuse such a comment at creation ([`super::add`] does), though
    /// never the gate.
    #[must_use]
    pub fn is_about_nothing(&self) -> bool {
        self.anchor.is_none() && self.context.is_none() && self.parent.is_none()
    }
}

/// Read the [`Comment`] stored at `tree`.
///
/// # Errors
///
/// A [`facet_git_tree::Error`] when `tree` is not a well-formed comment.
pub(crate) fn read_comment(tree: &ObjectId, objects: &impl Find) -> crate::Result<Comment> {
    Ok(facet_git_tree::deserialize::<Comment>(tree, objects)?)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, reason = "unit test")]

    use facet_git_tree::{ObjectStore, deserialize, serialize_into};
    use gix_object::Write as _;
    use rstest::rstest;

    use super::*;

    fn anchor_tree(store: &ObjectStore) -> RawTree {
        let tree = gix_object::Tree { entries: vec![] };
        RawTree::new(store.write(&tree).expect("tree"))
    }

    #[rstest]
    #[case::anchored_only(true, None, None)]
    #[case::context_only(false, Some("issues/42"), None)]
    #[case::reply_only(false, None, Some("abc123"))]
    #[case::every_kind_of_aboutness(true, Some("reviews/7"), Some("abc123"))]
    // @relation(model.comment, model.comment-state, model.comment-context, model.comment-thread, meta-ref.typed-tree, scope=function, role=Verifies)
    fn comment_round_trips_through_a_tree(
        #[case] anchored: bool,
        #[case] context: Option<&str>,
        #[case] parent: Option<&str>,
    ) {
        let store = ObjectStore::default();
        let comment = Comment {
            body: "looks off by one".to_owned(),
            state: "open".to_owned(),
            anchor: anchored.then(|| anchor_tree(&store)),
            context: context.map(str::to_owned),
            parent: parent.map(str::to_owned),
        };
        let root = serialize_into(&comment, &store).expect("serialize");
        let back: Comment = deserialize(&root, &store).expect("deserialize");
        assert_eq!(back, comment);
    }

    /// A tree that is not a well-formed comment fails to read.
    // @relation(model.comment, scope=function, role=Verifies)
    #[rstest]
    fn a_foreign_tree_fails_to_read() {
        let store = ObjectStore::default();
        let root = store
            .write(&gix_object::Tree { entries: vec![] })
            .expect("tree");
        let _error = read_comment(&root, &store).unwrap_err();
    }

    #[rstest]
    #[case::anchored(true, None, None, false)]
    #[case::contextual(false, Some("issues/42"), None, false)]
    #[case::reply(false, None, Some("abc"), false)]
    #[case::about_nothing(false, None, None, true)]
    // @relation(model.comment, scope=function, role=Verifies)
    fn is_about_nothing_requires_all_three_absent(
        #[case] anchored: bool,
        #[case] context: Option<&str>,
        #[case] parent: Option<&str>,
        #[case] expected: bool,
    ) {
        let store = ObjectStore::default();
        let comment = Comment {
            body: "b".to_owned(),
            state: "open".to_owned(),
            anchor: anchored.then(|| anchor_tree(&store)),
            context: context.map(str::to_owned),
            parent: parent.map(str::to_owned),
        };
        assert_eq!(comment.is_about_nothing(), expected);
    }
}
