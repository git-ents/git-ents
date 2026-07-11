//! The Issue entity: title, body, state, assignees, and labels.
//!
//! Spec coverage: `model.issue`.

use facet::Facet;

use crate::member::MemberId;

/// One issue, living at its own `refs/meta/issues/<id>` ref
/// (`namespace::issue_ref`, `meta-ref.granularity`).
///
/// `model.issue` requires `state` to accept custom values and `assignees`
/// to accept more than one member — "multiple assignees and custom states
/// are schema, not platform features" — so `state` is a plain `String`
/// rather than a fixed enum (contrast [`crate::result::Status`], which
/// *is* a fixed taxonomy because `model.result-taxonomy` says so
/// explicitly). Extending this struct with further fields later is a
/// storage migration (`model.extensibility`, `meta-ref.migration`), not a
/// platform request.
///
/// # Examples
///
/// ```
/// use ents_model::{Issue, MemberId};
///
/// let issue = Issue {
///     title: "gate rejects a valid signature".to_owned(),
///     body: "steps to reproduce...".to_owned(),
///     state: "triaged".to_owned(),
///     assignees: vec![MemberId::new("jdc")],
///     labels: vec!["bug".to_owned(), "gate".to_owned()],
/// };
/// let (id, store) = facet_git_tree::serialize(&issue).expect("serialize");
/// let back: Issue = facet_git_tree::deserialize(&id, &store).expect("deserialize");
/// assert_eq!(back, issue);
/// ```
// @relation(model.issue, meta-ref.typed-tree, model.extensibility, scope=file)
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct Issue {
    /// The issue's title.
    pub title: String,
    /// The issue's body.
    pub body: String,
    /// The issue's current state. Not a fixed enum: custom states are
    /// schema, not a platform feature (`model.issue`).
    pub state: String,
    /// The members assigned to the issue. More than one is ordinary.
    pub assignees: Vec<MemberId>,
    /// Free-form labels.
    pub labels: Vec<String>,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "unit test")]

    use facet_git_tree::{deserialize, serialize};
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::default_state_no_assignees("open", vec![], vec![])]
    #[case::custom_state_one_assignee("triaged", vec![MemberId::new("jdc")], vec!["bug".to_owned()])]
    #[case::custom_state_many_assignees(
        "needs-review",
        vec![MemberId::new("jdc"), MemberId::new("ci-worker")],
        vec!["bug".to_owned(), "gate".to_owned()]
    )]
    // @relation(model.issue, meta-ref.typed-tree, scope=function, role=Verifies)
    fn issue_round_trips_with_custom_state_and_any_assignee_count(
        #[case] state: &str,
        #[case] assignees: Vec<MemberId>,
        #[case] labels: Vec<String>,
    ) {
        let issue = Issue {
            title: "title".to_owned(),
            body: "body".to_owned(),
            state: state.to_owned(),
            assignees,
            labels,
        };
        let (id, store) = serialize(&issue).expect("serialize");
        let back: Issue = deserialize(&id, &store).expect("deserialize");
        assert_eq!(back, issue);
    }
}
