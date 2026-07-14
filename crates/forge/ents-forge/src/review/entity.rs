//! The Review entity: a verdict plus a context — the id of the most
//! recently reviewed commit, a verdict, and a body.
//!
//! Spec coverage: `model.review`.

use facet::Facet;
use gix_hash::ObjectId;

/// A review's verdict (`model.review`): a hard enum, unlike issue and
/// comment states — a verdict gates decisions, so its vocabulary is
/// platform, not schema.
///
/// Parses from and renders as its kebab-case convention names
/// (`approve`, `request-changes`, `comment`), the same strings every
/// surface shows.
///
/// # Examples
///
/// ```
/// use ents_forge::review::Verdict;
///
/// let verdict: Verdict = "request-changes".parse().expect("known verdict");
/// assert_eq!(verdict, Verdict::RequestChanges);
/// assert_eq!(verdict.to_string(), "request-changes");
/// assert!("needs-design-doc".parse::<Verdict>().is_err());
/// ```
// @relation(model.review, scope=type)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Facet)]
#[repr(u8)]
pub enum Verdict {
    /// The reviewed content is accepted.
    Approve,
    /// The reviewed content needs changes before acceptance.
    RequestChanges,
    /// Judgment withheld: the review exists for its body and thread.
    Comment,
}

impl std::str::FromStr for Verdict {
    type Err = crate::Error;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "approve" => Ok(Self::Approve),
            "request-changes" => Ok(Self::RequestChanges),
            "comment" => Ok(Self::Comment),
            other => Err(crate::Error::InvalidArgument(format!(
                "unknown verdict {other:?}: expected approve, request-changes, or comment"
            ))),
        }
    }
}

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Approve => "approve",
            Self::RequestChanges => "request-changes",
            Self::Comment => "comment",
        })
    }
}

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
/// (`model.review-pin`). The verdict is a hard [`Verdict`] enum — unlike
/// the open state vocabularies on [`crate::Issue`] and
/// [`crate::comment::Comment`], a verdict gates decisions, so its
/// vocabulary is platform, not schema. Reviewer and
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
/// let review = Review::new(target, ents_forge::review::Verdict::Approve, "looks good");
/// let (root, store) = facet_git_tree::serialize(&review).expect("serialize");
/// let back: Review = facet_git_tree::deserialize(&root, &store).expect("deserialize");
/// assert_eq!(back, review);
/// assert_eq!(back.target(), target);
/// ```
// @relation(model.review, meta-ref.identity-binding, meta-ref.typed-tree, model.extensibility, scope=file)
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct Review {
    target: [u8; 20],
    /// The review's verdict (`model.review`): a fixed [`Verdict`], not a
    /// string.
    pub verdict: Verdict,
    /// The review's body text.
    pub body: String,
}

impl Review {
    /// Build a review of `target` carrying `verdict` and `body`
    /// (`model.review`).
    #[must_use]
    pub fn new(target: ObjectId, verdict: Verdict, body: impl Into<String>) -> Self {
        let mut bytes = [0u8; 20];
        bytes.copy_from_slice(target.as_slice());
        Self {
            target: bytes,
            verdict,
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
    #[case::approve(Verdict::Approve)]
    #[case::request_changes(Verdict::RequestChanges)]
    #[case::comment(Verdict::Comment)]
    // @relation(model.review, meta-ref.typed-tree, scope=function, role=Verifies)
    fn review_round_trips_with_every_verdict(#[case] verdict: Verdict) {
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
        let review = Review::new(target, Verdict::Approve, "");
        assert_eq!(review.target(), target);
    }
}
