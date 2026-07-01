//! Reviews on a target (an issue, or eventually a proposal), sourced from the
//! `refs/meta/reviews/<target_id>/<reviewer_principal>` refs.
//!
//! One ref per reviewer per target: a reviewer's second look replaces their
//! own verdict on the same ref rather than appending a new one, while two
//! reviewers' verdicts never collide. `target_id` is the target's stable
//! genesis key ([`git_ents::issues::new_id`](crate::issues::new_id)), so a
//! review survives promotion exactly like a comment does.
//!
//! Merge readiness is computed at read time by aggregating review verdicts
//! with check runs — it is never stored, so there is no cached status to fall
//! out of sync with the reviews and runs it summarizes.

use std::path::Path;

use facet::Facet;

/// The namespace under which reviews are recorded: one ref,
/// `refs/meta/reviews/<target_id>/<reviewer_principal>`, per reviewer per
/// target.
pub const REVIEWS_NS: &str = "refs/meta/reviews";

/// A reviewer's verdict on a target.
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
#[repr(u8)]
pub enum Verdict {
    /// The reviewer approves the target as-is.
    Approve,
    /// The reviewer asks for changes before the target can land.
    RequestChanges,
    /// A non-blocking remark, neither an approval nor a change request.
    Comment,
}

/// One reviewer's review of a target, stored at
/// `refs/meta/reviews/<target_id>/<reviewer_principal>`.
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct Review {
    /// The reviewing member's principal — the ref's last segment.
    pub principal: String,
    /// The reviewer's verdict.
    pub verdict: Verdict,
    /// The reviewer's remarks.
    pub body: String,
}

/// The ref namespace holding every review of `target_id`.
fn target_reviews_ns(target_id: &str) -> String {
    format!("{REVIEWS_NS}/{target_id}")
}

/// Load `principal`'s review of `target_id` in `repo`.
pub fn load(
    repo: &Path,
    target_id: &str,
    principal: &str,
) -> Result<Option<Review>, git_store::Error> {
    git_store::Store::open(repo)?.load_item(&target_reviews_ns(target_id), principal)
}

/// Write `review` at `refs/meta/reviews/<target_id>/<review.principal>` in
/// `repo`, replacing that reviewer's prior verdict on `target_id` as a new
/// commit.
pub fn store(repo: &Path, target_id: &str, review: &Review) -> Result<(), git_store::Error> {
    git_store::Store::open(repo)?.store_item(
        &target_reviews_ns(target_id),
        &review.principal,
        review,
        "Add review",
    )
}

/// List every review of `target_id` in `repo`, as `(principal, review)` pairs,
/// newest first.
pub fn list(repo: &Path, target_id: &str) -> Result<Vec<(String, Review)>, git_store::Error> {
    git_store::Store::open(repo)?.list_items(&target_reviews_ns(target_id))
}

/// Whether `target_id` is ready to merge: aggregated at read time from its
/// recorded reviews and check runs, never stored.
///
/// Ready requires at least one [`Verdict::Approve`], no
/// [`Verdict::RequestChanges`], and every check in `runs`'s most recent run
/// (if any) having passed. A target with no runs recorded is judged on
/// reviews alone — checks that were never configured cannot block it.
#[must_use]
pub fn is_ready(reviews: &[Review], latest_run: Option<&crate::checks::Run>) -> bool {
    let approved = reviews.iter().any(|r| r.verdict == Verdict::Approve);
    let blocked = reviews.iter().any(|r| r.verdict == Verdict::RequestChanges);
    let checks_pass =
        latest_run.is_none_or(|run| run.results.iter().all(|outcome| outcome.outcome == "pass"));
    approved && !blocked && checks_pass
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::let_underscore_must_use,
        reason = "unit test"
    )]

    use super::*;
    use crate::checks::{Run, RunOutcome};
    use crate::testutil::unique_repo as new_repo;

    fn unique_repo() -> std::path::PathBuf {
        new_repo("reviews")
    }

    fn review(principal: &str, verdict: Verdict) -> Review {
        Review {
            principal: principal.to_owned(),
            verdict,
            body: "Looks good".to_owned(),
        }
    }

    #[test]
    fn store_then_load_round_trips_a_review() {
        let repo = unique_repo();
        let written = review("alice", Verdict::Approve);
        store(&repo, "target-1", &written).unwrap();
        assert_eq!(load(&repo, "target-1", "alice").unwrap(), Some(written));
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn a_second_review_from_the_same_reviewer_replaces_the_first() {
        let repo = unique_repo();
        store(&repo, "target-1", &review("alice", Verdict::RequestChanges)).unwrap();
        store(&repo, "target-1", &review("alice", Verdict::Approve)).unwrap();
        assert_eq!(
            load(&repo, "target-1", "alice").unwrap(),
            Some(review("alice", Verdict::Approve))
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn lists_only_the_reviews_on_the_named_target() {
        let repo = unique_repo();
        store(&repo, "target-1", &review("alice", Verdict::Approve)).unwrap();
        store(&repo, "target-1", &review("bob", Verdict::Comment)).unwrap();
        store(&repo, "target-2", &review("carol", Verdict::Approve)).unwrap();

        let mut principals: Vec<String> = list(&repo, "target-1")
            .unwrap()
            .into_iter()
            .map(|(principal, _review)| principal)
            .collect();
        principals.sort();
        assert_eq!(principals, vec!["alice".to_owned(), "bob".to_owned()]);
        let _ = std::fs::remove_dir_all(&repo);
    }

    fn outcome(name: &str, outcome: &str) -> RunOutcome {
        RunOutcome {
            name: name.to_owned(),
            outcome: outcome.to_owned(),
            duration_secs: None,
            log_url: None,
        }
    }

    #[test]
    fn ready_requires_an_approval() {
        assert!(!is_ready(&[], None));
        assert!(is_ready(&[review("alice", Verdict::Approve)], None));
    }

    #[test]
    fn a_requested_change_blocks_readiness_even_with_an_approval() {
        let reviews = [
            review("alice", Verdict::Approve),
            review("bob", Verdict::RequestChanges),
        ];
        assert!(!is_ready(&reviews, None));
    }

    #[test]
    fn a_failing_check_blocks_readiness() {
        let reviews = [review("alice", Verdict::Approve)];
        let run = Run {
            at: 0,
            results: vec![outcome("fmt", "pass"), outcome("test", "fail")],
        };
        assert!(!is_ready(&reviews, Some(&run)));
    }

    #[test]
    fn passing_checks_and_an_approval_are_ready() {
        let reviews = [review("alice", Verdict::Approve)];
        let run = Run {
            at: 0,
            results: vec![outcome("fmt", "pass"), outcome("test", "pass")],
        };
        assert!(is_ready(&reviews, Some(&run)));
    }
}
