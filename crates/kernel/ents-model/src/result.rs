//! The Result entity and its status taxonomy.
//!
//! Spec coverage: `model.result-taxonomy`, `model.result-identity`.

use facet::Facet;
use gix_hash::ObjectId;

/// The fixed set of outcomes a recorded result may carry.
///
/// `model.result-taxonomy` fixes this taxonomy at exactly three values;
/// unlike `ents-forge`'s `Issue::state`, which is intentionally open, this
/// is a closed enum precisely because the spec closes it. *When* each
/// status is written, and when nothing is written at all, is run
/// semantics specified by `effect.result-taxonomy` and owned by
/// `ents-effect` (phase 5) — this type only names the three values.
///
/// # Examples
///
/// ```
/// use ents_model::Status;
///
/// let (id, store) = facet_git_tree::serialize(&Status::Pass).expect("serialize");
/// let back: Status = facet_git_tree::deserialize(&id, &store).expect("deserialize");
/// assert_eq!(back, Status::Pass);
/// ```
// @relation(model.result-taxonomy, meta-ref.typed-tree, model.extensibility, scope=file)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Facet)]
#[repr(u8)]
pub enum Status {
    /// The effect ran and succeeded.
    Pass,
    /// The effect ran and reported failure.
    Fail,
    /// The effect could not complete a run (as distinct from completing
    /// and reporting failure).
    Error,
}

/// A recorded result: the outcome of running one effect against one
/// commit, living at `refs/meta/results/<effect>/<short-oid>`
/// (`namespace::result_ref`) or the self-run mirror.
///
/// `model.result-identity` requires the result to carry the effect's name
/// and the full oid of the commit the run judged *as tree fields*, from
/// which the refname's `<effect>` and `<short-oid>` segments derive
/// (`meta-ref.identity-binding`): the gate recomputes the refname from
/// these fields and refuses a mismatch, so a signed `pass` cannot be
/// replayed as the result of a different effect or commit — a result means
/// something with the refname stripped away. The composite key freezes the
/// genesis tree by identity, so this struct evolves additively only.
///
/// `target` is stored as a raw 20-byte SHA-1 array, the same
/// `facet-git-tree`-native oid representation [`crate::Redaction`] uses;
/// [`ResultRecord::new`] and [`ResultRecord::target`] keep the public API
/// in gitoxide's own type. The fields are not parent edges: a result ref's
/// parents stay prior states of the same result, and a result never
/// retains the judged commit's ancestry the way a pin does
/// (`model.result-identity`, `model.review-pin`).
///
/// # Examples
///
/// ```
/// use ents_model::{ResultRecord, Status};
///
/// let target = gix_hash::ObjectId::null(gix_hash::Kind::Sha1);
/// let result = ResultRecord::new("unit", target, Status::Pass);
/// assert_eq!(result.effect, "unit");
/// assert_eq!(result.target(), target);
///
/// let (id, store) = facet_git_tree::serialize(&result).expect("serialize");
/// let back: ResultRecord = facet_git_tree::deserialize(&id, &store).expect("deserialize");
/// assert_eq!(back, result);
/// ```
// @relation(model.result-identity, meta-ref.identity-binding, meta-ref.typed-tree, model.extensibility, scope=file)
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct ResultRecord {
    /// The name of the effect this result records — binds the refname's
    /// `<effect>` segment (`model.result-identity`).
    pub effect: String,
    /// The full oid of the commit the run judged, as a raw 20-byte SHA-1
    /// array — binds the refname's `<short-oid>` segment
    /// (`model.result-identity`).
    target: [u8; 20],
    /// The run's outcome.
    pub status: Status,
}

impl ResultRecord {
    /// Record `status` for `effect` against the commit `target`.
    #[must_use]
    pub fn new(effect: impl Into<String>, target: ObjectId, status: Status) -> Self {
        let mut bytes = [0u8; 20];
        bytes.copy_from_slice(target.as_slice());
        Self {
            effect: effect.into(),
            target: bytes,
            status,
        }
    }

    /// The oid of the commit this result judged.
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
    #[case::pass(Status::Pass)]
    #[case::fail(Status::Fail)]
    #[case::error(Status::Error)]
    // @relation(model.result-taxonomy, meta-ref.typed-tree, scope=function, role=Verifies)
    fn every_taxonomy_value_round_trips(#[case] status: Status) {
        let (id, store) = serialize(&status).expect("serialize");
        let back: Status = deserialize(&id, &store).expect("deserialize");
        assert_eq!(back, status);
    }

    #[rstest]
    #[case::pass(Status::Pass)]
    #[case::fail(Status::Fail)]
    #[case::error(Status::Error)]
    // @relation(model.result-identity, meta-ref.typed-tree, scope=function, role=Verifies)
    fn result_round_trips_and_preserves_effect_and_target(#[case] status: Status) {
        let target = ObjectId::from_bytes_or_panic(&[9u8; 20]);
        let result = ResultRecord::new("unit", target, status);
        let (id, store) = serialize(&result).expect("serialize");
        let back: ResultRecord = deserialize(&id, &store).expect("deserialize");
        assert_eq!(back, result);
        assert_eq!(back.effect, "unit");
        assert_eq!(back.target(), target);
    }
}
