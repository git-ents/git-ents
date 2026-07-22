//! The `agent-review` effect's run path (`docs/agent-sessions-plan.adoc`'s
//! Phase 5): auto-opening a follow-on [`ents_forge::review::Review`] once
//! an `agent-exec` run finishes `Done` with a result branch, for a session
//! whose confirm froze [`ents_forge::agent::ReviewPolicy::Auto`].
//!
//! # Why this lives here, not in `ents-effect` or `ents-forge`
//!
//! Exactly [`crate::agent_worker`]'s and [`crate::plan_worker`]'s own
//! reasoning: `ents-effect` links exactly `ents-model`, `ents-query`, and
//! `ents-receive` (`docs/spec/overview.adoc`'s crate-graph table) — never
//! `ents-forge`, from either side — so a function that needs
//! [`ents_forge::agent`]'s typed session reads and
//! [`ents_forge::review::Review`]'s own shape at once cannot live in either
//! kernel crate. `git-ents` already depends on both, so this is the same
//! "session handler" composition-root seam [`crate::hook::post_receive`]
//! installs [`crate::agent_worker`] and [`crate::plan_worker`] for,
//! installed here for the one other effect name ([`AGENT_REVIEW_NAME`])
//! that needs bespoke handling.
//!
//! # No sandbox at all, unlike `agent-exec` and `agent-plan`
//!
//! Opening a review is pure repository mutation — a signed commit onto the
//! review's own entity ref plus its retention pin
//! (`model.review`, `model.review-pin`) — never a sandboxed command. This
//! module's own [`run_agent_review`] takes no `Executor`, no toolchains, no
//! `run` command, and no scratch directory: unlike
//! [`crate::agent_worker::run_agent_exec`] and
//! [`crate::plan_worker::run_agent_plan`], there is nothing here for an
//! [`ents_effect::Executor`] to do. [`ents_effect::definition::agent_review`]'s
//! own doc explains why its canonical [`ents_model::Effect`] definition
//! still carries (empty/inert) `toolchains`/`run` fields despite that: the
//! shape every effect definition carries, never consulted by this handler.
//!
//! # What the dequeued oid already is
//!
//! `AGENT_REVIEW_TRIGGER`'s own `results(agent-exec, pass)` semantics
//! (`query.results`) resolve to the *tested* commit — the session tip
//! `agent_worker::run_agent_exec` read as its own dispatched oid — never
//! the result ref's own commit. This module never decodes the `agent-exec`
//! `ResultRecord` itself: the dequeued `oid` already *is* that tested
//! commit, so walking it to genesis ([`genesis_of`]) recovers the session
//! id directly, exactly as `agent_worker`'s and `plan_worker`'s own
//! identically-named private copies do.
//!
//! # Idempotency
//!
//! One result, one obligation
//! (`docs/agent-sessions-plan.adoc`'s Phase 5: "idempotent by
//! construction"): before ever proposing a review, this module checks
//! whether the exact review this effect would open —
//! `refs/meta/reviews/<result-branch-tip>/<reviewer>`, the same
//! deterministic composite key every review occupies
//! (`model.review`) — already exists, and no-ops if so. A worker retry
//! that re-dequeues the same `oid` a second time (this effect's own results
//! ref did not yet land before the process died) finds that ref already
//! there and takes the same no-op path, so re-running the handler twice
//! never yields a second review.

use ents_forge::agent::{ReviewDispatch, dispatch_review};
use ents_forge::review::{Review, Verdict};
use ents_model::MemberId;
use ents_receive::{EventSink, Identity, Mode, Outcome, TxResult};
use gix_hash::ObjectId;
use gix_object::{CommitRef, Find, Kind, Write};
use gix_ref_store::RefStore;

use crate::error::{Error, Result};

/// `agent-review`'s own effect name — re-exported so a caller deciding
/// which bespoke handler a dequeued `(effect, oid)` obligation belongs to
/// does not need a second import of `ents_effect::definition`.
pub const AGENT_REVIEW_NAME: &str = ents_effect::definition::AGENT_REVIEW_NAME;

/// What running the `agent-review` effect against one dequeued
/// `(agent-review, oid)` obligation did.
#[derive(Debug)]
pub enum AgentReviewOutcome {
    /// The session's confirm did not freeze `Auto`, or it has no result
    /// branch recorded (`ReviewDispatch::NoOp`): a cheap `pass` was
    /// recorded to discharge the obligation, no review opened.
    NoOp,
    /// The review this effect would open already exists — either an
    /// earlier run of this same effect got as far as opening it before a
    /// worker died short of recording its own result (the idempotency case
    /// this module's own doc describes), or a manual reviewer opened one
    /// under this same identity first: a `pass` was recorded, and no
    /// second review commit was proposed.
    AlreadyOpen {
        /// The session's own genesis-oid id.
        id: String,
    },
    /// This worker opened the review, atomically with its retention pin.
    Opened {
        /// The session's own genesis-oid id.
        id: String,
        /// The result branch tip the opened review targets.
        target: ObjectId,
        /// The review-and-pin proposal's own outcome.
        outcome: Outcome,
    },
}

/// Run the `agent-review` effect against the single dequeued commit `oid`
/// — the tested commit `agent-exec`'s own `pass` result names (see this
/// module's own doc for why that already is the session tip to walk to
/// genesis, never the result ref's commit).
///
/// `reviewer` becomes both the opened review's composite-key `<member>`
/// segment and the identity `sign`/`author` sign as
/// (`gate.owner-mutation`'s own rule for `Namespace::Review`: "a review
/// advances only under the signature of the member its refname names," with
/// no carve-out for genesis) — the same worker identity
/// [`crate::agent_worker::run_agent_exec`]'s own `worker` parameter signs
/// the session's claim and finish as.
///
/// # Errors
///
/// Any [`Error`] from reading or decoding the session, resolving the result
/// branch's own ref, or building and sending the review-and-pin proposal.
#[expect(
    clippy::too_many_arguments,
    reason = "one input per identity/materialization step, mirrors run_agent_exec's and \
              run_agent_plan's own identically-justified shape, minus the sandbox-only \
              parameters this effect never needs"
)]
pub fn run_agent_review<O>(
    refs: &dyn RefStore,
    objects: &O,
    events: &dyn EventSink,
    oid: ObjectId,
    reviewer: MemberId,
    author: &gix::actor::Signature,
    sign: &dyn Fn(&[u8]) -> String,
    mode: Mode,
) -> Result<AgentReviewOutcome>
where
    O: Find + Write,
{
    let results_ref =
        ents_model::namespace::result_ref(AGENT_REVIEW_NAME, &ents_effect::run::short_oid(oid))?;
    let id = genesis_of(objects, oid)?.to_string();
    let session = ents_forge::agent::show(refs, objects, &id)?;

    if dispatch_review(&session) == ReviewDispatch::NoOp {
        record_pass(refs, objects, events, &results_ref, oid, author, sign, mode)?;
        return Ok(AgentReviewOutcome::NoOp);
    }

    // `ReviewDispatch::Open` guarantees a result branch is recorded.
    let branch_name = session.meta.result_branch.clone().ok_or_else(|| {
        Error::InvalidArgument(format!(
            "agent session {id} dispatched to open a review with no result branch recorded"
        ))
    })?;
    let branch_ref: gix::refs::FullName =
        format!("refs/heads/{branch_name}")
            .try_into()
            .map_err(|_source| {
                Error::InvalidArgument(format!(
                    "agent session {id}'s result branch {branch_name:?} is not a well-formed \
                     refname"
                ))
            })?;
    let target = refs
        .get(branch_ref.as_ref())?
        .ok_or_else(|| Error::NotFound {
            what: branch_name.clone(),
        })?;

    let review_ref = ents_model::namespace::review_ref(&target.to_string(), &reviewer)?;
    if refs.get(review_ref.as_ref())?.is_some() {
        record_pass(refs, objects, events, &results_ref, oid, author, sign, mode)?;
        return Ok(AgentReviewOutcome::AlreadyOpen { id });
    }

    let pin_ref = ents_model::namespace::review_pin_ref(&target.to_string(), &reviewer)?;
    let review = Review::new(
        target,
        Verdict::Comment,
        format!(
            "Auto-opened by the agent-review effect: agent session {id}'s agent-exec run \
             landed at {oid}."
        ),
    );
    let identity = Identity {
        actor: author.clone(),
        author: None,
        sign,
    };
    let outcome = ents_receive::propose_entity_with_pin(
        refs,
        objects,
        events,
        review_ref,
        &review,
        pin_ref,
        target,
        &identity,
        &format!("Auto-open review of agent session {id}'s result"),
        &format!("Pin review {target}/{reviewer}"),
        mode,
    )?;
    if outcome.result != TxResult::Applied {
        // Lost a race against another writer that opened the identical
        // review between this worker's own pre-check above and this write
        // — discharge the obligation exactly like the already-open case,
        // never propose a second, conflicting review commit.
        record_pass(refs, objects, events, &results_ref, oid, author, sign, mode)?;
        return Ok(AgentReviewOutcome::AlreadyOpen { id });
    }

    record_pass(refs, objects, events, &results_ref, oid, author, sign, mode)?;
    Ok(AgentReviewOutcome::Opened {
        id,
        target,
        outcome,
    })
}

/// Record a cheap `pass` for `oid` on the canonical `agent-review` results
/// ref — the no-op and already-open paths, and the discharge that follows
/// a successful open, all funnel through this one call.
#[expect(
    clippy::too_many_arguments,
    reason = "one input per write_result parameter; a thin, single-call wrapper, mirroring \
              agent_worker's and plan_worker's identically-shaped copies"
)]
fn record_pass<O: Find + Write>(
    refs: &dyn RefStore,
    objects: &O,
    events: &dyn EventSink,
    results_ref: &gix::refs::FullName,
    oid: ObjectId,
    author: &gix::actor::Signature,
    sign: &dyn Fn(&[u8]) -> String,
    mode: Mode,
) -> Result<Outcome> {
    Ok(ents_effect::write_result(
        refs,
        objects,
        events,
        results_ref.clone(),
        AGENT_REVIEW_NAME,
        oid,
        ents_model::Status::Pass,
        author,
        sign,
        mode,
    )?)
}

/// The oldest ancestor of `oid` reachable by following each commit's first
/// parent — the session's own genesis oid and id; duplicated from
/// `agent_worker`'s own private copy (that module's own doc names this
/// codebase's accepted pattern for a small per-module copy).
fn genesis_of(objects: &impl Find, oid: ObjectId) -> Result<ObjectId> {
    let mut current = oid;
    loop {
        let mut buf = Vec::new();
        let data = objects
            .try_find(&current, &mut buf)
            .map_err(|source| Error::InvalidArgument(source.to_string()))?
            .ok_or_else(|| Error::NotFound {
                what: current.to_string(),
            })?;
        if data.kind != Kind::Commit {
            return Err(Error::NotFound {
                what: current.to_string(),
            });
        }
        let commit = CommitRef::from_bytes(data.data, current.kind())
            .map_err(|source| Error::InvalidArgument(source.to_string()))?;
        match commit.parents().next() {
            Some(parent) => current = parent,
            None => return Ok(current),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        clippy::unwrap_used,
        clippy::panic,
        reason = "unit test; the panic is an assertion on a `let else` branch"
    )]

    use ents_forge::agent::{ClaimAgentSession, FinishAgentSession, FinishOutcome, ReviewPolicy};
    use ents_gate::Config;
    use ents_model::{Provenance, namespace};
    use ents_receive::NullEventSink;
    use ents_testutil::{Keypair, MemRefStore, ObjectStore, advance_ref, enroll_member};
    use gix_ref_store::RefStoreRead as _;
    use rstest::rstest;

    use super::*;

    type Signer = Box<dyn Fn(&[u8]) -> String>;

    struct Fixture {
        refs: MemRefStore,
        objects: ObjectStore,
        sign: Signer,
    }

    impl Fixture {
        fn new() -> Self {
            let refs = MemRefStore::default();
            let objects = ObjectStore::default();
            let key = Keypair::from_seed(1);
            let sign = Keypair::from_seed(1);
            enroll_member(
                &refs,
                &objects,
                "worker",
                &key,
                Provenance::AdminRegistered,
                100,
            );
            let config_ref: gix::refs::FullName = namespace::CONFIG_REF.try_into().expect("valid");
            ents_testutil::write_meta_entity(
                &refs,
                &objects,
                config_ref,
                &Config {
                    epoch: Some(150),
                    ..Config::default()
                },
                Some(&key),
                150,
            );
            advance_ref(&refs, &objects, "refs/heads/main", 1, 200);
            Self {
                refs,
                objects,
                sign: Box::new(move |payload: &[u8]| sign.sign(payload)),
            }
        }

        fn identity(&self) -> Identity<'_> {
            Identity {
                actor: self.author(),
                author: None,
                sign: &*self.sign,
            }
        }

        fn author(&self) -> gix::actor::Signature {
            gix::actor::Signature {
                name: "worker".into(),
                email: "worker@ents.test".into(),
                time: gix::date::Time {
                    seconds: 1_000,
                    offset: 0,
                },
            }
        }

        fn sign_fn(&self) -> &dyn Fn(&[u8]) -> String {
            &*self.sign
        }

        fn reviewer() -> MemberId {
            MemberId::new("worker")
        }

        /// A confirmed session, driven through claim and finish to `Done`
        /// with a real, pushed result branch — [`run_agent_review`]'s own
        /// `ReviewDispatch::Open` precondition, parameterized by the
        /// review policy frozen at confirm.
        fn done_session(&self, policy: ReviewPolicy, branch: &str) -> (ObjectId, String) {
            let identity = self.identity();
            let (id, outcome) = ents_forge::agent::new(
                &self.refs,
                &self.objects,
                &NullEventSink,
                ents_forge::agent::NewAgentSession {
                    member: MemberId::new("jdc"),
                    prompt: "fix the flaky test".to_owned(),
                    model: "claude-sonnet-5".to_owned(),
                    toolchains: vec![],
                    base_ref: "refs/heads/main".to_owned(),
                    review_policy: policy,
                    retry_of: None,
                },
                &identity,
                Mode::Advisory,
            )
            .expect("creates");
            assert_eq!(outcome.result, TxResult::Applied);

            ents_forge::agent::revise_plan(
                &self.refs,
                &self.objects,
                &NullEventSink,
                &id,
                "do the thing".to_owned(),
                &identity,
                Mode::Advisory,
            )
            .expect("revises");
            ents_forge::agent::confirm(
                &self.refs,
                &self.objects,
                &NullEventSink,
                &id,
                None,
                &identity,
                Mode::Advisory,
            )
            .expect("confirms");

            let ref_name = ents_model::namespace::agent_session_ref(&id).expect("valid");
            let queued_tip = self
                .refs
                .get(ref_name.as_ref())
                .expect("readable")
                .expect("exists");

            ents_forge::agent::claim(
                &self.refs,
                &self.objects,
                &NullEventSink,
                &id,
                ClaimAgentSession {
                    worker: MemberId::new("worker"),
                    sprite: "sprite-1".to_owned(),
                },
                &identity,
                Mode::Advisory,
            )
            .expect("claims");
            ents_forge::agent::finish(
                &self.refs,
                &self.objects,
                &NullEventSink,
                &id,
                FinishAgentSession {
                    outcome: FinishOutcome::Done,
                    result_branch: Some(branch.to_owned()),
                    thread: vec![b"transcript".to_vec()],
                },
                &identity,
                Mode::Advisory,
            )
            .expect("finishes");

            // A real `refs/heads/<branch>` tip for the review to target --
            // `run_agent_review` resolves it directly off `refs`, never
            // through a real on-disk `gix::open` (see this module's own
            // doc: the review's `target` field is raw bytes, never an
            // object this store must itself carry).
            advance_ref(
                &self.refs,
                &self.objects,
                &format!("refs/heads/{branch}"),
                1,
                300,
            );

            (queued_tip, id)
        }
    }

    #[rstest]
    // @relation(scope=function, role=Verifies)
    fn a_manual_session_is_a_cheap_no_op() {
        let fixture = Fixture::new();
        let (oid, id) = fixture.done_session(ReviewPolicy::Manual, "agent/jdc/deadbee1");

        let author = fixture.author();
        let run = run_agent_review(
            &fixture.refs,
            &fixture.objects,
            &NullEventSink,
            oid,
            Fixture::reviewer(),
            &author,
            fixture.sign_fn(),
            Mode::Advisory,
        )
        .expect("runs");
        assert!(matches!(run, AgentReviewOutcome::NoOp));

        let results_ref =
            ents_model::namespace::result_ref(AGENT_REVIEW_NAME, &ents_effect::run::short_oid(oid))
                .expect("valid");
        assert!(
            fixture
                .refs
                .get(results_ref.as_ref())
                .expect("readable")
                .is_some(),
            "the no-op path must still discharge the obligation with a recorded pass"
        );

        let session = ents_forge::agent::show(&fixture.refs, &fixture.objects, &id).expect("shows");
        let branch_ref: gix::refs::FullName = format!(
            "refs/heads/{}",
            session.meta.result_branch.expect("branch recorded")
        )
        .try_into()
        .expect("valid");
        let target_hex = format!(
            "{}",
            fixture
                .refs
                .get(branch_ref.as_ref())
                .expect("readable")
                .expect("branch tip exists")
        );
        let review_ref =
            ents_model::namespace::review_ref(&target_hex, &Fixture::reviewer()).expect("valid");
        assert!(
            fixture
                .refs
                .get(review_ref.as_ref())
                .expect("readable")
                .is_none(),
            "manual must never open a review"
        );
    }

    #[rstest]
    // @relation(model.review, model.review-pin, receive.multi-ref-atomicity, scope=function, role=Verifies)
    fn an_auto_session_opens_exactly_one_review() {
        let fixture = Fixture::new();
        let (oid, id) = fixture.done_session(ReviewPolicy::Auto, "agent/jdc/deadbee2");

        let author = fixture.author();
        let run = run_agent_review(
            &fixture.refs,
            &fixture.objects,
            &NullEventSink,
            oid,
            Fixture::reviewer(),
            &author,
            fixture.sign_fn(),
            Mode::Advisory,
        )
        .expect("runs");
        let AgentReviewOutcome::Opened {
            id: opened_id,
            target,
            outcome,
        } = run
        else {
            panic!("expected Opened, got {run:?}");
        };
        assert_eq!(opened_id, id);
        assert_eq!(outcome.result, TxResult::Applied);

        let review_ref =
            ents_model::namespace::review_ref(&target.to_string(), &Fixture::reviewer())
                .expect("valid");
        let review_tip = fixture
            .refs
            .get(review_ref.as_ref())
            .expect("readable")
            .expect("review landed");
        let tree = crate::commands::commit_tree(&fixture.objects, review_tip).expect("tree");
        let review: ents_forge::review::Review =
            facet_git_tree::deserialize(&tree, &fixture.objects).expect("decodes");
        assert_eq!(review.target(), target);
        assert_eq!(review.verdict, ents_forge::review::Verdict::Comment);

        let pin_ref =
            ents_model::namespace::review_pin_ref(&target.to_string(), &Fixture::reviewer())
                .expect("valid");
        assert!(
            fixture
                .refs
                .get(pin_ref.as_ref())
                .expect("readable")
                .is_some(),
            "the retention pin must land atomically with the review"
        );

        let results_ref =
            ents_model::namespace::result_ref(AGENT_REVIEW_NAME, &ents_effect::run::short_oid(oid))
                .expect("valid");
        assert!(
            fixture
                .refs
                .get(results_ref.as_ref())
                .expect("readable")
                .is_some(),
            "opening a review must still discharge this effect's own obligation"
        );
    }

    /// Running the handler twice on the same dequeued oid (a worker retry)
    /// must yield exactly one review -- `docs/agent-sessions-plan.adoc`'s
    /// Phase 5 acceptance, and the red test for this module's own
    /// idempotency claim.
    #[rstest]
    // @relation(scope=function, role=Verifies)
    fn a_worker_retry_yields_exactly_one_review() {
        let fixture = Fixture::new();
        let (oid, _id) = fixture.done_session(ReviewPolicy::Auto, "agent/jdc/deadbee3");
        let author = fixture.author();

        let first = run_agent_review(
            &fixture.refs,
            &fixture.objects,
            &NullEventSink,
            oid,
            Fixture::reviewer(),
            &author,
            fixture.sign_fn(),
            Mode::Advisory,
        )
        .expect("runs");
        assert!(matches!(first, AgentReviewOutcome::Opened { .. }));

        let second = run_agent_review(
            &fixture.refs,
            &fixture.objects,
            &NullEventSink,
            oid,
            Fixture::reviewer(),
            &author,
            fixture.sign_fn(),
            Mode::Advisory,
        )
        .expect("a retry is not an error");
        let AgentReviewOutcome::AlreadyOpen { .. } = second else {
            panic!("expected AlreadyOpen on retry, got {second:?}");
        };

        let session =
            ents_forge::agent::show(&fixture.refs, &fixture.objects, &_id).expect("shows");
        let branch_ref: gix::refs::FullName = format!(
            "refs/heads/{}",
            session.meta.result_branch.expect("branch recorded")
        )
        .try_into()
        .expect("valid");
        let target = fixture
            .refs
            .get(branch_ref.as_ref())
            .expect("readable")
            .expect("branch tip exists");
        let review_ref =
            ents_model::namespace::review_ref(&target.to_string(), &Fixture::reviewer())
                .expect("valid");

        // Exactly one review commit exists: the ref's own tip has no
        // parent (a fresh genesis, never a second commit stacked onto it).
        let review_tip = fixture
            .refs
            .get(review_ref.as_ref())
            .expect("readable")
            .expect("review landed");
        let mut buf = Vec::new();
        let data = fixture
            .objects
            .try_find(&review_tip, &mut buf)
            .expect("readable")
            .expect("exists");
        let commit = CommitRef::from_bytes(data.data, review_tip.kind()).expect("decodes");
        assert_eq!(
            commit.parents().count(),
            0,
            "a retry must never stack a second commit onto the review ref"
        );
    }
}
