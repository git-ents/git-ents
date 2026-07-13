//! The Review entity: a verdict plus a context — the id of the most
//! recently reviewed commit, a verdict, and a body.
//!
//! Spec coverage: `model.review`.

use facet::Facet;
use gix_hash::ObjectId;

/// A verdict on a commit, plus a body (`model.review`).
///
/// Every review occupies exactly two refs: this entity's own tree at
/// `refs/meta/reviews/<target>/<member>`, and a retention pin at
/// `refs/meta/pins/reviews/<target>/<member>` anchoring the reviewed content
/// itself (`model.review-pin`) — [`super::new`] writes both. `target` is
/// the oid of the most recently reviewed commit, stored as a plain data
/// field the same way [`ents_model::ResultRecord`]'s own `target` field
/// stores the commit it judged: a `[u8; 20]` field plus a [`Review::target`]
/// accessor, so reading it back never requires the pin ref — the pin
/// anchors reachability, the entity describes what was reviewed. At
/// genesis this field equals the refname's `<target>` segment and binds
/// it (`meta-ref.identity-binding`); re-reviewing a descendant advances
/// this field while the refname stays keyed by genesis
/// (`model.review-pin`). `approve` and `request-changes` are conventions,
/// not an enum: custom verdicts are schema, not a platform feature
/// (`model.extensibility`), exactly as custom states are for
/// [`crate::Issue`] and [`crate::comment::Comment`]. Reviewer and
/// timestamp come from the mutation commit chain rather than a stored
/// field (`meta-ref.identity-binding`), so `Review` carries no author or
/// timestamp field — the same omission [`crate::comment::Comment`] makes.
/// A review's discussion is [`crate::comment::Comment`] entities naming
/// the review as their context (`model.comment-context`); `Review` itself
/// stores no list of its comments.
///
/// # Examples
///
/// ```
/// use ents_forge::review::Review;
///
/// let target = gix_hash::ObjectId::from_hex(b"0123456789abcdef0123456789abcdef01234567")
///     .expect("valid hex");
/// let review = Review::new(target, "approve", "looks good");
/// let (root, store) = facet_git_tree::serialize(&review).expect("serialize");
/// let back: Review = facet_git_tree::deserialize(&root, &store).expect("deserialize");
/// assert_eq!(back, review);
/// assert_eq!(back.target(), target);
/// ```
// @relation(model.review, meta-ref.identity-binding, meta-ref.typed-tree, model.extensibility, scope=file)
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct Review {
    target: [u8; 20],
    /// The review's verdict — `approve`, `request-changes`, or any custom
    /// value a schema defines; not a fixed enum (`model.review`,
    /// `model.extensibility`).
    pub verdict: String,
    /// The review's body text.
    pub body: String,
}

impl Review {
    /// Build a review of `target` carrying `verdict` and `body`
    /// (`model.review`).
    #[must_use]
    pub fn new(target: ObjectId, verdict: impl Into<String>, body: impl Into<String>) -> Self {
        let mut bytes = [0u8; 20];
        bytes.copy_from_slice(target.as_slice());
        Self {
            target: bytes,
            verdict: verdict.into(),
            body: body.into(),
        }
    }

    /// The id of the most recently reviewed commit (`model.review`):
    /// reading this never requires the pin ref
    /// (`refs/meta/pins/reviews/<target>/<member>`) — the pin anchors
    /// reachability, the entity describes what was reviewed.
    #[must_use]
    pub fn target(&self) -> ObjectId {
        ObjectId::from_bytes_or_panic(&self.target)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "unit test")]

    use facet_git_tree::{deserialize, serialize};
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::approve("approve")]
    #[case::request_changes("request-changes")]
    #[case::custom_verdict("needs-design-doc")]
    // @relation(model.review, model.extensibility, meta-ref.typed-tree, scope=function, role=Verifies)
    fn review_round_trips_with_any_verdict_string(#[case] verdict: &str) {
        let target =
            ObjectId::from_hex(b"0123456789abcdef0123456789abcdef01234567").expect("valid hex");
        let review = Review::new(target, verdict, "reviewed the change");
        let (root, store) = serialize(&review).expect("serialize");
        let back: Review = deserialize(&root, &store).expect("deserialize");
        assert_eq!(back, review);
        assert_eq!(back.target(), target);
    }

    #[rstest]
    // @relation(model.review, scope=function, role=Verifies)
    fn target_accessor_reflects_the_stored_bytes() {
        let target =
            ObjectId::from_hex(b"fedcba9876543210fedcba9876543210fedcba98").expect("valid hex");
        let review = Review::new(target, "approve", "");
        assert_eq!(review.target(), target);
    }
}
