//! The runner's claim-or-no-op decision (`docs/agent-sessions-plan.adoc`'s
//! Phase 2, "the runner inspects the tip and records a cheap `pass` no-op
//! unless the tip is queued-and-unclaimed"): a pure function from a
//! decoded [`AgentSession`] tip to what a dequeued `(agent-exec, oid)` pair
//! should do next.
//!
//! This lives in `ents-forge`, alongside [`AgentSession`] itself, rather
//! than in `ents-effect`: `ents-effect`'s own `Cargo.toml` depends on
//! exactly `ents-model`, `ents-query`, and `ents-receive` (mirrored in
//! `docs/spec/overview.adoc`'s crate-graph table), and `ents-forge` is not
//! among them — adding it would be a new edge the spec's crate graph does
//! not name, for a decision that needs nothing `ents-effect` carries.
//! `ents-forge` already depends on none of `ents-effect`'s own crates in
//! the wrong direction either, so this stays a same-crate function next to
//! the type it decides over, with no new cross-crate dependency at all.

use super::{AgentSession, ReviewPolicy, Status};

/// What a dequeued `(agent-exec, oid)` pair resolves to once the runner
/// reads the agent session tip at `oid`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dispatch {
    /// The tip is queued and unclaimed
    /// ([`AgentSession::queued`]): the runner should CAS a `Running` status
    /// commit ([`super::command::claim`]) — first worker wins, losers
    /// no-op on the same tip once it is no longer queued.
    Claim,
    /// Anything else — planning, awaiting confirmation, already running,
    /// or a terminal state: a cheap `pass` no-op, no state change.
    NoOp,
}

/// Decide [`Dispatch`] for `session`'s current tip — queued-and-unclaimed
/// maps to [`Dispatch::Claim`], everything else to [`Dispatch::NoOp`].
///
/// # Examples
///
/// ```
/// use ents_forge::agent::{AgentSession, Dispatch, ReviewPolicy, SessionMeta, dispatch};
/// use ents_model::MemberId;
///
/// let mut session = AgentSession {
///     meta: SessionMeta::new(
///         MemberId::new("jdc"), 1_000, "claude-sonnet-5", vec![],
///         "refs/heads/main", ReviewPolicy::Manual, None,
///     ),
///     plan: None,
///     confirm: None,
///     thread: vec![],
/// };
/// assert_eq!(dispatch(&session), Dispatch::NoOp, "planning, no plan yet");
///
/// session.plan = Some("do the thing".to_owned());
/// session.meta.status = ents_forge::agent::Status::Ready;
/// assert_eq!(dispatch(&session), Dispatch::NoOp, "awaiting confirmation");
/// ```
#[must_use]
pub fn dispatch(session: &AgentSession) -> Dispatch {
    if session.queued() {
        Dispatch::Claim
    } else {
        Dispatch::NoOp
    }
}

/// What a dequeued `(agent-plan, oid)` pair resolves to once the runner
/// reads the agent session tip at `oid`
/// (`docs/agent-sessions-plan.adoc`'s Phase 4): headless plan drafting
/// fires iff the session is [`Status::Planning`], carries a prompt (a
/// non-empty [`AgentSession::thread`], seeded by [`super::command::new`]),
/// and has no plan leaf yet ([`AgentSession::plan`] is `None`) — everything
/// else, including a session already `Ready` awaiting confirmation, one
/// already running, or one that is `Planning` but has no prompt at all
/// (unreachable through [`super::command::new`], but not through this
/// predicate), is a cheap `pass` no-op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanDispatch {
    /// The tip is `Planning`, has a prompt, and has no plan yet: the
    /// runner should draft one ([`super::command::draft_plan`]).
    Draft,
    /// Anything else: a cheap `pass` no-op, no state change.
    NoOp,
}

/// Decide [`PlanDispatch`] for `session`'s current tip.
///
/// # Examples
///
/// ```
/// use ents_forge::agent::{PlanDispatch, ReviewPolicy, SessionMeta, dispatch_plan};
/// use ents_model::MemberId;
///
/// let mut session = ents_forge::agent::AgentSession {
///     meta: SessionMeta::new(
///         MemberId::new("jdc"), 1_000, "claude-sonnet-5", vec![],
///         "refs/heads/main", ReviewPolicy::Manual, None,
///     ),
///     plan: None,
///     confirm: None,
///     thread: vec![],
/// };
/// assert_eq!(dispatch_plan(&session), PlanDispatch::NoOp, "no prompt yet");
///
/// session.thread.push(b"fix the flaky test".to_vec());
/// assert_eq!(dispatch_plan(&session), PlanDispatch::Draft);
///
/// session.plan = Some("do the thing".to_owned());
/// assert_eq!(dispatch_plan(&session), PlanDispatch::NoOp, "already has a plan");
/// ```
#[must_use]
pub fn dispatch_plan(session: &AgentSession) -> PlanDispatch {
    if session.meta.status == Status::Planning
        && !session.thread.is_empty()
        && session.plan.is_none()
    {
        PlanDispatch::Draft
    } else {
        PlanDispatch::NoOp
    }
}

/// What a dequeued `(agent-review, oid)` pair resolves to once the runner
/// reads the agent session tip at `oid` (`docs/agent-sessions-plan.adoc`'s
/// Phase 5): auto-review opens a follow-on review iff the session reached
/// [`Status::Done`] with a result branch recorded and its
/// [`super::Confirm::review_policy`] — the policy actually frozen at
/// confirm time, never [`super::SessionMeta::review_policy`]'s own
/// possibly-since-changed value — is [`ReviewPolicy::Auto`]. Everything
/// else (`Manual`, no result branch, or a `Done` session that somehow
/// carries no confirm at all, unreachable through
/// [`super::command::confirm`] but not through this predicate) is a cheap
/// `pass` no-op, review-opening left to the member
/// (`docs/agent-sessions-plan.adoc`'s Phase 5, "manual yields none").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewDispatch {
    /// The runner should open (or confirm it already opened, idempotency
    /// judged downstream by the review's own deterministic refname) a
    /// review of the session's result branch.
    Open,
    /// Anything else: a cheap `pass` no-op, no review opened.
    NoOp,
}

/// Decide [`ReviewDispatch`] for `session`'s current tip.
///
/// # Examples
///
/// ```
/// use ents_forge::agent::{Confirm, ReviewDispatch, ReviewPolicy, SessionMeta, dispatch_review};
/// use ents_model::MemberId;
///
/// let mut session = ents_forge::agent::AgentSession {
///     meta: SessionMeta::new(
///         MemberId::new("jdc"), 1_000, "claude-sonnet-5", vec![],
///         "refs/heads/main", ReviewPolicy::Auto, None,
///     ),
///     plan: Some("do the thing".to_owned()),
///     confirm: Some(Confirm::new(
///         gix_hash::ObjectId::from_bytes_or_panic(&[7u8; 20]),
///         ReviewPolicy::Auto,
///     )),
///     thread: vec![],
/// };
/// session.meta.status = ents_forge::agent::Status::Done;
/// assert_eq!(dispatch_review(&session), ReviewDispatch::NoOp, "no result branch yet");
///
/// session.meta.result_branch = Some("agent/jdc/abc1234".to_owned());
/// assert_eq!(dispatch_review(&session), ReviewDispatch::Open);
/// ```
#[must_use]
pub fn dispatch_review(session: &AgentSession) -> ReviewDispatch {
    let auto = session
        .confirm
        .as_ref()
        .is_some_and(|confirm| confirm.review_policy == ReviewPolicy::Auto);
    if session.meta.status == Status::Done && session.meta.result_branch.is_some() && auto {
        ReviewDispatch::Open
    } else {
        ReviewDispatch::NoOp
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "unit test")]

    use rstest::rstest;

    use super::*;
    use crate::agent::{Confirm, FailureReason, ReviewPolicy, SessionMeta};

    fn session(status: Status, plan: Option<&str>, confirm: Option<Confirm>) -> AgentSession {
        let mut meta = SessionMeta::new(
            ents_model::MemberId::new("jdc"),
            1_000,
            "claude-sonnet-5",
            vec![],
            "refs/heads/main",
            ReviewPolicy::Manual,
            None,
        );
        meta.status = status;
        AgentSession {
            meta,
            plan: plan.map(str::to_owned),
            confirm,
            thread: vec![],
        }
    }

    /// A [`Confirm`] binding `plan`'s own git blob hash — the same content
    /// hash [`AgentSession::plan_hash`] computes internally, recomputed
    /// here since that accessor is the only way this test crosses into it
    /// (mirroring `entity`'s own `blob_hash` test helper).
    fn confirm_for(plan: &str) -> Confirm {
        confirm_with(plan, ReviewPolicy::Manual)
    }

    /// [`confirm_for`], generalized over the frozen review policy --
    /// `dispatch_review`'s own tests need both variants, not just
    /// `Manual`.
    fn confirm_with(plan: &str, policy: ReviewPolicy) -> Confirm {
        let hash = gix_object::compute_hash(
            gix_hash::Kind::Sha1,
            gix_object::Kind::Blob,
            plan.as_bytes(),
        )
        .expect("hashing an in-memory byte slice cannot fail");
        Confirm::new(hash, policy)
    }

    #[rstest]
    #[case::planning_no_plan(Status::Planning, None, None, Dispatch::NoOp)]
    #[case::ready_awaiting_confirmation(Status::Ready, Some("do the thing"), None, Dispatch::NoOp)]
    #[case::running(Status::Running, Some("do the thing"), None, Dispatch::NoOp)]
    #[case::done(Status::Done, Some("do the thing"), None, Dispatch::NoOp)]
    #[case::failed(
        Status::Failed(FailureReason { detail: "sandbox died".to_owned() }),
        Some("do the thing"),
        None,
        Dispatch::NoOp
    )]
    // @relation(scope=function, role=Verifies)
    fn dispatch_is_no_op_outside_queued(
        #[case] status: Status,
        #[case] plan: Option<&str>,
        #[case] confirm: Option<Confirm>,
        #[case] expected: Dispatch,
    ) {
        assert_eq!(dispatch(&session(status, plan, confirm)), expected);
    }

    #[rstest]
    // @relation(scope=function, role=Verifies)
    fn dispatch_claims_a_queued_session() {
        let plan = "do the thing";
        let session = session(Status::Ready, Some(plan), Some(confirm_for(plan)));
        assert!(session.queued());
        assert_eq!(dispatch(&session), Dispatch::Claim);
    }

    #[rstest]
    // @relation(scope=function, role=Verifies)
    fn dispatch_is_no_op_when_a_confirm_binds_a_stale_plan_hash() {
        let stale = confirm_for("an earlier draft");
        let session = session(
            Status::Ready,
            Some("a materially different plan"),
            Some(stale),
        );
        assert!(session.awaiting_confirmation());
        assert_eq!(dispatch(&session), Dispatch::NoOp);
    }

    // ---------------------------------------------------------------
    // `dispatch_plan`: `docs/agent-sessions-plan.adoc`'s Phase 4.
    // ---------------------------------------------------------------

    #[rstest]
    // @relation(scope=function, role=Verifies)
    fn dispatch_plan_drafts_a_planning_session_with_a_prompt_and_no_plan() {
        let mut planning_with_prompt = session(Status::Planning, None, None);
        planning_with_prompt
            .thread
            .push(b"fix the flaky test".to_vec());
        assert_eq!(dispatch_plan(&planning_with_prompt), PlanDispatch::Draft);
    }

    #[rstest]
    // @relation(scope=function, role=Verifies)
    fn dispatch_plan_is_no_op_with_no_prompt_at_all() {
        let planning_no_prompt = session(Status::Planning, None, None);
        assert!(planning_no_prompt.thread.is_empty());
        assert_eq!(dispatch_plan(&planning_no_prompt), PlanDispatch::NoOp);
    }

    #[rstest]
    #[case::ready(Status::Ready)]
    #[case::running(Status::Running)]
    #[case::done(Status::Done)]
    #[case::failed(Status::Failed(FailureReason { detail: "oops".to_owned() }))]
    // @relation(scope=function, role=Verifies)
    fn dispatch_plan_is_no_op_outside_planning(#[case] status: Status) {
        let mut with_prompt = session(status, None, None);
        with_prompt.thread.push(b"fix the flaky test".to_vec());
        assert_eq!(dispatch_plan(&with_prompt), PlanDispatch::NoOp);
    }

    #[rstest]
    // @relation(scope=function, role=Verifies)
    fn dispatch_plan_is_no_op_once_a_plan_already_exists() {
        let mut already_planned = session(Status::Planning, Some("do the thing"), None);
        already_planned.thread.push(b"fix the flaky test".to_vec());
        assert_eq!(dispatch_plan(&already_planned), PlanDispatch::NoOp);
    }

    // ---------------------------------------------------------------
    // `dispatch_review`: `docs/agent-sessions-plan.adoc`'s Phase 5.
    // ---------------------------------------------------------------

    /// A `Done` session whose confirm froze `Auto` and carries a result
    /// branch opens.
    #[rstest]
    // @relation(scope=function, role=Verifies)
    fn dispatch_review_opens_a_done_auto_session_with_a_result_branch() {
        let plan = "do the thing";
        let confirm = confirm_with(plan, ReviewPolicy::Auto);
        let mut done = session(Status::Done, Some(plan), Some(confirm));
        done.meta.result_branch = Some("agent/jdc/abc1234".to_owned());
        assert_eq!(dispatch_review(&done), ReviewDispatch::Open);
    }

    /// `Manual` never opens, whether or not a result branch exists.
    #[rstest]
    // @relation(scope=function, role=Verifies)
    fn dispatch_review_is_no_op_for_manual() {
        let plan = "do the thing";
        let confirm = confirm_with(plan, ReviewPolicy::Manual);
        let mut done = session(Status::Done, Some(plan), Some(confirm));
        done.meta.result_branch = Some("agent/jdc/abc1234".to_owned());
        assert_eq!(dispatch_review(&done), ReviewDispatch::NoOp);
    }

    /// `Auto`, but no result branch was ever recorded (a `Done` session
    /// whose run somehow never pushed one) — nothing to review yet.
    #[rstest]
    // @relation(scope=function, role=Verifies)
    fn dispatch_review_is_no_op_without_a_result_branch() {
        let plan = "do the thing";
        let confirm = confirm_with(plan, ReviewPolicy::Auto);
        let done = session(Status::Done, Some(plan), Some(confirm));
        assert!(done.meta.result_branch.is_none());
        assert_eq!(dispatch_review(&done), ReviewDispatch::NoOp);
    }

    /// `Auto`, with a result branch, but not yet `Done` (still `Running`) --
    /// nothing to review before the run finishes.
    #[rstest]
    #[case::planning(Status::Planning)]
    #[case::ready(Status::Ready)]
    #[case::running(Status::Running)]
    #[case::failed(Status::Failed(FailureReason { detail: "oops".to_owned() }))]
    // @relation(scope=function, role=Verifies)
    fn dispatch_review_is_no_op_outside_done(#[case] status: Status) {
        let plan = "do the thing";
        let confirm = confirm_with(plan, ReviewPolicy::Auto);
        let mut session = session(status, Some(plan), Some(confirm));
        session.meta.result_branch = Some("agent/jdc/abc1234".to_owned());
        assert_eq!(dispatch_review(&session), ReviewDispatch::NoOp);
    }

    /// `Done`, a result branch recorded, but no confirm at all on the tip
    /// (unreachable through the command layer, not through this predicate)
    /// — no policy to read as `Auto`, so no open.
    #[rstest]
    // @relation(scope=function, role=Verifies)
    fn dispatch_review_is_no_op_with_no_confirm_at_all() {
        let mut done = session(Status::Done, Some("do the thing"), None);
        done.meta.result_branch = Some("agent/jdc/abc1234".to_owned());
        assert_eq!(dispatch_review(&done), ReviewDispatch::NoOp);
    }

    /// [`SessionMeta::review_policy`] having since drifted to `Auto` must
    /// never override what [`Confirm::review_policy`] actually froze --
    /// only the confirm's own field is consulted.
    #[rstest]
    // @relation(scope=function, role=Verifies)
    fn dispatch_review_reads_the_confirms_frozen_policy_not_the_metas_current_one() {
        let plan = "do the thing";
        let confirm = confirm_with(plan, ReviewPolicy::Manual);
        let mut done = session(Status::Done, Some(plan), Some(confirm));
        done.meta.result_branch = Some("agent/jdc/abc1234".to_owned());
        done.meta.review_policy = ReviewPolicy::Auto;
        assert_eq!(
            dispatch_review(&done),
            ReviewDispatch::NoOp,
            "meta.review_policy drifting to auto must not resurrect a manual confirm"
        );
    }
}
