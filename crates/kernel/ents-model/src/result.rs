//! The Result status taxonomy.
//!
//! Spec coverage: `model.result-taxonomy`.

use facet::Facet;

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
}
