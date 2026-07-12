//! The Comment entity: a body about something — an anchor, a context
//! entity, a parent comment, or any combination.
//!
//! Spec coverage: `model.comment`, `model.comment-state`,
//! `model.comment-context`, `model.comment-thread`, `meta-ref.migration`.

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
/// (`meta-ref.trailers`) — `Comment` therefore has no author or timestamp
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
/// Comment trees written before this struct broadened (a bare
/// `{body, anchor}` shape) still read back through
/// [`read_comment`]'s legacy fallback (`meta-ref.migration`); mutating one
/// rewrites its tree under this struct as an ordinary commit on top of the
/// old tip, keeping the old encoding as archive.
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
    /// comment belongs to, such as `issues/<id>` or `reviews/<id>`
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

/// The comment tree shape phase-7 code wrote: a body and a mandatory,
/// directly-embedded anchor — no state, context, or parent. Kept only as
/// [`read_comment`]'s fallback target (`meta-ref.migration`: history keeps
/// the old encoding as archive, and the tip of a pre-migration ref *is*
/// still this encoding until something mutates it).
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub(crate) struct LegacyComment {
    pub(crate) body: String,
    pub(crate) anchor: RawTree,
}

impl From<LegacyComment> for Comment {
    fn from(legacy: LegacyComment) -> Self {
        Self {
            body: legacy.body,
            state: "open".to_owned(),
            anchor: Some(legacy.anchor),
            context: None,
            parent: None,
        }
    }
}

/// Read the [`Comment`] stored at `tree`, falling back to the legacy
/// `{body, anchor}` shape (`meta-ref.migration`).
///
/// The two encodings are structurally disjoint, so no version marker is
/// consulted (`meta-ref.typed-tree` forbids one in the tree, and the
/// reserved `Schema-Version:` trailer stays unused while detection works
/// structurally): a legacy tree has no `state` entry and embeds its anchor
/// tree directly where the broadened struct expects an `Option` wrapper,
/// so it can never be misread as a current [`Comment`] — and a current tree
/// always carries `state`, so it is never consulted against the legacy
/// shape at all.
///
/// A legacy read maps to the broadened struct exactly as the migration
/// commit would rewrite it: state `open` (`model.comment-state`'s value
/// for every comment created before states existed), no context, no
/// parent.
///
/// # Errors
///
/// The current shape's own [`facet_git_tree::Error`] when `tree` reads as
/// neither encoding.
// @relation(meta-ref.migration, scope=function)
pub(crate) fn read_comment(tree: &ObjectId, objects: &impl Find) -> crate::Result<Comment> {
    match facet_git_tree::deserialize::<Comment>(tree, objects) {
        Ok(comment) => Ok(comment),
        Err(error) => match facet_git_tree::deserialize::<LegacyComment>(tree, objects) {
            Ok(legacy) => Ok(legacy.into()),
            // Report the *current* shape's failure: a tree that is neither
            // encoding is diagnosed against the schema in force.
            Err(_legacy_error) => Err(error.into()),
        },
    }
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

    /// `meta-ref.migration`: a tree written by phase-7 code — the bare
    /// `{body, anchor}` shape — still reads back, mapping to state `open`
    /// with no context or parent; a current tree reads as itself.
    // @relation(meta-ref.migration, scope=function, role=Verifies)
    #[rstest]
    fn legacy_trees_still_read_back() {
        let store = ObjectStore::default();
        let legacy = LegacyComment {
            body: "written by phase-7 code".to_owned(),
            anchor: anchor_tree(&store),
        };
        let root = serialize_into(&legacy, &store).expect("serialize");

        let read = read_comment(&root, &store).expect("legacy fallback reads");
        assert_eq!(read.body, legacy.body);
        assert_eq!(read.state, "open");
        assert_eq!(read.anchor, Some(legacy.anchor));
        assert_eq!(read.context, None);
        assert_eq!(read.parent, None);
    }

    /// A tree that is neither encoding fails against the schema in force,
    /// not the archival one.
    // @relation(meta-ref.migration, scope=function, role=Verifies)
    #[rstest]
    fn a_foreign_tree_reads_as_neither_encoding() {
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
