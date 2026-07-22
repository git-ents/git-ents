//! Integration coverage for the `agent` command layer
//! (`docs/agent-sessions-plan.adoc`'s Phase 1): genesis dedup under a
//! same-second double submit, plan revision dropping a stale confirm, and
//! the guards around `confirm`/`revise_plan`.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "integration test: fixtures panic on setup failure"
)]

use ents_forge::agent::{
    self, ClaimAgentSession, FailureReason, FinishAgentSession, FinishOutcome, NewAgentSession,
    ReviewPolicy, Status,
};
use ents_model::MemberId;
use ents_receive::{Identity, Mode, NullEventSink, TxResult};
use ents_testutil::{Keypair, MemRefStore, ObjectStore};
use rstest::rstest;

/// A detached signer over some bytes, returning an armored signature.
type Signer = Box<dyn Fn(&[u8]) -> String>;

struct Fixture {
    refs: MemRefStore,
    objects: ObjectStore,
    sign: Signer,
}

impl Fixture {
    fn new() -> Self {
        let key = Keypair::from_seed(1);
        Self {
            refs: MemRefStore::default(),
            objects: ObjectStore::default(),
            sign: Box::new(move |payload| key.sign(payload)),
        }
    }

    /// The same identity every call in a test uses — same actor, same
    /// timestamp, same (deterministic) signer — the precondition the
    /// same-second double-submit test relies on.
    fn identity(&self) -> Identity<'_> {
        Identity {
            actor: gix::actor::Signature {
                name: "test".into(),
                email: "test@ents.test".into(),
                time: gix::date::Time {
                    seconds: 1_000,
                    offset: 0,
                },
            },
            author: None,
            sign: &*self.sign,
        }
    }

    fn draft(&self) -> NewAgentSession {
        NewAgentSession {
            member: MemberId::new("jdc"),
            prompt: "fix the flaky test".to_owned(),
            model: "claude-sonnet-5".to_owned(),
            toolchains: vec![],
            base_ref: "refs/heads/main".to_owned(),
            review_policy: ReviewPolicy::Manual,
            retry_of: None,
        }
    }

    fn new_session(&self) -> String {
        let (id, outcome) = agent::new(
            &self.refs,
            &self.objects,
            &NullEventSink,
            self.draft(),
            &self.identity(),
            Mode::Advisory,
        )
        .expect("creates");
        assert_eq!(outcome.result, TxResult::Applied);
        id
    }

    fn revise_plan(&self, id: &str, text: &str) -> ents_receive::Outcome {
        agent::revise_plan(
            &self.refs,
            &self.objects,
            &NullEventSink,
            id,
            text.to_owned(),
            &self.identity(),
            Mode::Advisory,
        )
        .expect("revises")
    }

    fn confirm(&self, id: &str) -> ents_receive::Outcome {
        agent::confirm(
            &self.refs,
            &self.objects,
            &NullEventSink,
            id,
            None,
            &self.identity(),
            Mode::Advisory,
        )
        .expect("confirms")
    }

    /// A session revised and confirmed against its own plan — `queued`,
    /// the only precondition [`agent::claim`] accepts.
    fn queued_session(&self) -> String {
        let id = self.new_session();
        self.revise_plan(&id, "do the thing");
        self.confirm(&id);
        id
    }

    fn claim(&self, id: &str) -> ents_forge::Result<ents_receive::Outcome> {
        agent::claim(
            &self.refs,
            &self.objects,
            &NullEventSink,
            id,
            ClaimAgentSession {
                worker: MemberId::new("worker"),
                sprite: "sprite-1".to_owned(),
            },
            &self.identity(),
            Mode::Advisory,
        )
    }

    fn finish(
        &self,
        id: &str,
        finish: FinishAgentSession,
    ) -> ents_forge::Result<ents_receive::Outcome> {
        agent::finish(
            &self.refs,
            &self.objects,
            &NullEventSink,
            id,
            finish,
            &self.identity(),
            Mode::Advisory,
        )
    }
}

// ---------------------------------------------------------------------
// meta-ref.identity-binding: genesis dedup, no nonce.
// ---------------------------------------------------------------------

/// Two identical submissions built from the same fields and the same
/// (same-second) identity serialize to byte-identical genesis commits, so
/// they dedupe to exactly one session ref rather than minting a second —
/// idempotent creation with no nonce anywhere.
// @relation(meta-ref.identity-binding, scope=function, role=Verifies)
#[rstest]
fn a_same_second_double_submit_produces_one_session() {
    let fixture = Fixture::new();

    let (first_id, first_outcome) = agent::new(
        &fixture.refs,
        &fixture.objects,
        &NullEventSink,
        fixture.draft(),
        &fixture.identity(),
        Mode::Advisory,
    )
    .expect("creates");
    assert_eq!(first_outcome.result, TxResult::Applied);

    let (second_id, second_outcome) = agent::new(
        &fixture.refs,
        &fixture.objects,
        &NullEventSink,
        fixture.draft(),
        &fixture.identity(),
        Mode::Advisory,
    )
    .expect("creates");
    assert_eq!(second_outcome.result, TxResult::Applied);

    assert_eq!(
        first_id, second_id,
        "identical same-second submissions must derive the same genesis oid"
    );
    let (sessions, unreadable) = agent::list_all(&fixture.refs, &fixture.objects).expect("lists");
    assert!(unreadable.is_empty());
    assert_eq!(
        sessions.len(),
        1,
        "the duplicate submit must not mint a second session ref"
    );
}

/// A submission with a different field (a different prompt, here) is a
/// different genesis entirely — dedup only ever collapses byte-identical
/// content, never merely-similar submissions.
// @relation(meta-ref.identity-binding, scope=function, role=Verifies)
#[rstest]
fn a_different_submission_is_a_different_session() {
    let fixture = Fixture::new();
    let (first_id, _) = agent::new(
        &fixture.refs,
        &fixture.objects,
        &NullEventSink,
        fixture.draft(),
        &fixture.identity(),
        Mode::Advisory,
    )
    .expect("creates");

    let mut second_draft = fixture.draft();
    second_draft.prompt = "a completely different task".to_owned();
    let (second_id, _) = agent::new(
        &fixture.refs,
        &fixture.objects,
        &NullEventSink,
        second_draft,
        &fixture.identity(),
        Mode::Advisory,
    )
    .expect("creates");

    assert_ne!(first_id, second_id);
    let sessions = agent::list(&fixture.refs, &fixture.objects).expect("lists");
    assert_eq!(sessions.len(), 2);
}

// ---------------------------------------------------------------------
// Plan revision drops a stale confirm.
// ---------------------------------------------------------------------

/// Confirming binds the plan hash and transitions the session to `queued`;
/// revising the plan afterward drops the confirm unconditionally, returning
/// the session to `awaiting confirmation` — never leaving a confirm bound
/// to text that no longer exists.
// @relation(scope=function, role=Verifies)
#[rstest]
fn revising_the_plan_drops_the_confirm_bound_to_the_old_text() {
    let fixture = Fixture::new();
    let id = fixture.new_session();

    fixture.revise_plan(&id, "first draft of the plan");
    let confirm_outcome = agent::confirm(
        &fixture.refs,
        &fixture.objects,
        &NullEventSink,
        &id,
        None,
        &fixture.identity(),
        Mode::Advisory,
    )
    .expect("confirms");
    assert_eq!(confirm_outcome.result, TxResult::Applied);

    let queued = agent::show(&fixture.refs, &fixture.objects, &id).expect("shows");
    assert!(queued.queued());
    assert!(!queued.awaiting_confirmation());

    let revise_outcome = fixture.revise_plan(&id, "a materially different plan");
    assert_eq!(revise_outcome.result, TxResult::Applied);

    let revised = agent::show(&fixture.refs, &fixture.objects, &id).expect("shows");
    assert_eq!(revised.plan.as_deref(), Some("a materially different plan"));
    assert!(
        revised.confirm.is_none(),
        "a plan revision must drop the stale confirm leaf, not merely let it read as stale"
    );
    assert!(revised.awaiting_confirmation());
    assert!(!revised.queued());
}

// ---------------------------------------------------------------------
// Guards: confirm and revise_plan refuse outside their preconditions.
// ---------------------------------------------------------------------

/// `confirm` refuses a session with no plan yet.
// @relation(scope=function, role=Verifies)
#[rstest]
fn confirm_refuses_a_session_with_no_plan() {
    let fixture = Fixture::new();
    let id = fixture.new_session();
    let error = agent::confirm(
        &fixture.refs,
        &fixture.objects,
        &NullEventSink,
        &id,
        None,
        &fixture.identity(),
        Mode::Advisory,
    )
    .expect_err("refused");
    assert!(matches!(error, ents_forge::Error::InvalidArgument(_)));
}

/// `confirm` refuses a session whose plan is empty, or all-whitespace —
/// `docs/agent-sessions-plan.adoc`'s Phase 4 acceptance: "no confirm can
/// bind an empty or absent plan leaf."
// @relation(scope=function, role=Verifies)
#[rstest]
#[case::empty("")]
#[case::whitespace("   \n\t")]
fn confirm_refuses_a_session_with_an_empty_plan(#[case] empty_plan: &str) {
    let fixture = Fixture::new();
    let id = fixture.new_session();
    fixture.revise_plan(&id, empty_plan);

    let error = agent::confirm(
        &fixture.refs,
        &fixture.objects,
        &NullEventSink,
        &id,
        None,
        &fixture.identity(),
        Mode::Advisory,
    )
    .expect_err("refused");
    assert!(matches!(error, ents_forge::Error::InvalidArgument(_)));
}

/// `revise_plan` refuses a session once it is past the point of no return
/// (`Running`, `Done`, or `Failed`) — seeded directly onto the ref, since no
/// Phase 1 command reaches those statuses yet (Phase 2's effect worker
/// does).
// @relation(scope=function, role=Verifies)
#[rstest]
#[case::running(Status::Running)]
#[case::done(Status::Done)]
#[case::failed(Status::Failed(FailureReason { detail: "sandbox died".to_owned() }))]
fn revise_plan_refuses_a_session_past_the_point_of_no_return(#[case] status: Status) {
    use ents_forge::agent::{AgentSession, SessionMeta};

    let fixture = Fixture::new();
    let mut meta = SessionMeta::new(
        MemberId::new("jdc"),
        1_000,
        "claude-sonnet-5",
        vec![],
        "refs/heads/main",
        ReviewPolicy::Manual,
        None,
    );
    meta.status = status;
    let session = AgentSession {
        meta,
        plan: Some("an existing plan".to_owned()),
        confirm: None,
        thread: vec![b"turn one".to_vec()],
    };
    let refname = ents_model::namespace::agent_session_ref("deadbeef").expect("valid");
    ents_testutil::write_meta_entity(
        &fixture.refs,
        &fixture.objects,
        refname,
        &session,
        None,
        900,
    );

    let error = agent::revise_plan(
        &fixture.refs,
        &fixture.objects,
        &NullEventSink,
        "deadbeef",
        "a redraft".to_owned(),
        &fixture.identity(),
        Mode::Advisory,
    )
    .expect_err("refused");
    assert!(matches!(error, ents_forge::Error::InvalidArgument(_)));
}

/// `new` refuses a toolchain name with no `refs/meta/toolchains/*` ref.
// @relation(scope=function, role=Verifies)
#[rstest]
fn new_refuses_an_unknown_toolchain() {
    let fixture = Fixture::new();
    let mut draft = fixture.draft();
    draft.toolchains = vec!["no-such-toolchain".to_owned()];
    let error = agent::new(
        &fixture.refs,
        &fixture.objects,
        &NullEventSink,
        draft,
        &fixture.identity(),
        Mode::Advisory,
    )
    .expect_err("refused");
    assert!(matches!(error, ents_forge::Error::NotFound { .. }));
}

// ---------------------------------------------------------------------
// `claim` and `finish` (`docs/agent-sessions-plan.adoc`'s Phase 2a): the
// guards around advancing to `Running` and to a terminal state.
// ---------------------------------------------------------------------

/// `claim` refuses a session that is not queued: still `planning` (no
/// plan at all), and `ready` but awaiting confirmation (a plan with no
/// confirm bound to it).
// @relation(scope=function, role=Verifies)
#[rstest]
fn claim_refuses_a_session_that_is_not_queued() {
    let fixture = Fixture::new();

    let planning = fixture.new_session();
    let error = fixture
        .claim(&planning)
        .expect_err("refused: still planning");
    assert!(matches!(error, ents_forge::Error::InvalidArgument(_)));

    fixture.revise_plan(&planning, "do the thing");
    let error = fixture
        .claim(&planning)
        .expect_err("refused: awaiting confirmation, not queued");
    assert!(matches!(error, ents_forge::Error::InvalidArgument(_)));
}

/// `claim` on a queued session advances it to `Running`, recording the
/// worker, the sprite name, and the claim's own timestamp as `started`.
// @relation(scope=function, role=Verifies)
#[rstest]
fn claim_advances_a_queued_session_to_running_with_worker_sprite_and_started() {
    let fixture = Fixture::new();
    let id = fixture.queued_session();

    let outcome = fixture.claim(&id).expect("claims");
    assert_eq!(outcome.result, TxResult::Applied);

    let session = agent::show(&fixture.refs, &fixture.objects, &id).expect("shows");
    assert_eq!(session.meta.status, Status::Running);
    assert_eq!(session.meta.worker, Some(MemberId::new("worker")));
    assert_eq!(session.meta.sprite.as_deref(), Some("sprite-1"));
    assert_eq!(session.meta.started, Some(1_000));
    assert!(!session.queued(), "Running is past queued");
}

/// A second `claim` against an already-claimed session refuses at the
/// command layer: the first claim already advanced the session past
/// `queued`, so the ordinary precondition check refuses it — first worker
/// wins, the loser gets an ordinary [`ents_forge::Error::InvalidArgument`],
/// never a second `Running` write.
// @relation(scope=function, role=Verifies)
#[rstest]
fn a_second_claim_refuses_at_the_command_layer() {
    let fixture = Fixture::new();
    let id = fixture.queued_session();
    fixture.claim(&id).expect("first claim succeeds");

    let error = fixture.claim(&id).expect_err("refused: no longer queued");
    assert!(matches!(error, ents_forge::Error::InvalidArgument(_)));

    // The session still carries the first claim's worker, untouched by
    // the refused second attempt.
    let session = agent::show(&fixture.refs, &fixture.objects, &id).expect("shows");
    assert_eq!(session.meta.worker, Some(MemberId::new("worker")));
}

/// `finish` refuses a session that was never claimed (`planning`,
/// `ready`/awaiting-confirmation, and `ready`/queued) — only a `Running`
/// session may be finished.
// @relation(scope=function, role=Verifies)
#[rstest]
fn finish_refuses_a_session_that_is_not_running() {
    let fixture = Fixture::new();
    let done = FinishAgentSession {
        outcome: FinishOutcome::Done,
        result_branch: None,
        thread: vec![],
    };

    let planning = fixture.new_session();
    let error = fixture
        .finish(&planning, done.clone())
        .expect_err("refused: still planning");
    assert!(matches!(error, ents_forge::Error::InvalidArgument(_)));

    let queued = fixture.queued_session();
    let error = fixture
        .finish(&queued, done)
        .expect_err("refused: queued, never claimed");
    assert!(matches!(error, ents_forge::Error::InvalidArgument(_)));
}

/// `finish` refuses a session that already reached a terminal state —
/// `finish` may not be called twice.
// @relation(scope=function, role=Verifies)
#[rstest]
fn finish_refuses_a_session_already_finished() {
    let fixture = Fixture::new();
    let id = fixture.queued_session();
    fixture.claim(&id).expect("claims");
    fixture
        .finish(
            &id,
            FinishAgentSession {
                outcome: FinishOutcome::Done,
                result_branch: None,
                thread: vec![],
            },
        )
        .expect("finishes");

    let error = fixture
        .finish(
            &id,
            FinishAgentSession {
                outcome: FinishOutcome::Done,
                result_branch: None,
                thread: vec![],
            },
        )
        .expect_err("refused: already done");
    assert!(matches!(error, ents_forge::Error::InvalidArgument(_)));
}

/// `finish` from `Running` with `Done` records the finished timestamp, the
/// result branch, and appends the execution transcript to `thread/`.
// @relation(scope=function, role=Verifies)
#[rstest]
fn finish_done_records_branch_timestamp_and_appends_the_transcript() {
    let fixture = Fixture::new();
    let id = fixture.queued_session();
    fixture.claim(&id).expect("claims");

    let before = agent::show(&fixture.refs, &fixture.objects, &id).expect("shows");
    let turns_before = before.thread.len();

    let outcome = fixture
        .finish(
            &id,
            FinishAgentSession {
                outcome: FinishOutcome::Done,
                result_branch: Some(format!("agent/jdc/{id}")),
                thread: vec![b"turn: ran the fix".to_vec()],
            },
        )
        .expect("finishes");
    assert_eq!(outcome.result, TxResult::Applied);

    let session = agent::show(&fixture.refs, &fixture.objects, &id).expect("shows");
    assert_eq!(session.meta.status, Status::Done);
    assert_eq!(session.meta.finished, Some(1_000));
    assert_eq!(session.meta.result_branch, Some(format!("agent/jdc/{id}")));
    assert_eq!(session.thread.len(), turns_before.saturating_add(1));
    assert_eq!(
        session.thread.last().map(Vec::as_slice),
        Some(b"turn: ran the fix".as_slice())
    );
}

/// `finish` from `Running` with `Failed` records the failure reason as the
/// session's terminal state.
// @relation(scope=function, role=Verifies)
#[rstest]
fn finish_failed_records_the_failure_reason() {
    let fixture = Fixture::new();
    let id = fixture.queued_session();
    fixture.claim(&id).expect("claims");

    fixture
        .finish(
            &id,
            FinishAgentSession {
                outcome: FinishOutcome::Failed("sandbox died".to_owned()),
                result_branch: None,
                thread: vec![],
            },
        )
        .expect("finishes");

    let session = agent::show(&fixture.refs, &fixture.objects, &id).expect("shows");
    assert_eq!(
        session.meta.status,
        Status::Failed(FailureReason {
            detail: "sandbox died".to_owned()
        })
    );
}

// ---------------------------------------------------------------------
// Phase 4 (`docs/agent-sessions-plan.adoc`): `reopen`, `append_thread`,
// `draft_plan`.
// ---------------------------------------------------------------------

/// `reopen` returns a queued session to `planning`, dropping the confirm —
/// the plan's resolved-by-default "un-queue" item, offered as its own
/// action rather than only as `revise_plan`'s side effect.
// @relation(scope=function, role=Verifies)
#[rstest]
fn reopen_returns_a_queued_session_to_planning_and_drops_the_confirm() {
    let fixture = Fixture::new();
    let id = fixture.queued_session();

    let outcome = agent::reopen(
        &fixture.refs,
        &fixture.objects,
        &NullEventSink,
        &id,
        &fixture.identity(),
        Mode::Advisory,
    )
    .expect("reopens");
    assert_eq!(outcome.result, TxResult::Applied);

    let session = agent::show(&fixture.refs, &fixture.objects, &id).expect("shows");
    assert_eq!(session.meta.status, Status::Planning);
    assert!(session.confirm.is_none());
    assert!(
        session.plan.is_some(),
        "reopening keeps the plan text around for the resumed conversation's context"
    );
}

/// `reopen` refuses a session that is not `Ready`: still `planning` has
/// nothing to reopen, and `running`/terminal are past the point of no
/// return.
// @relation(scope=function, role=Verifies)
#[rstest]
fn reopen_refuses_a_session_that_is_not_ready() {
    let fixture = Fixture::new();
    let planning = fixture.new_session();
    let error = agent::reopen(
        &fixture.refs,
        &fixture.objects,
        &NullEventSink,
        &planning,
        &fixture.identity(),
        Mode::Advisory,
    )
    .expect_err("refused: still planning");
    assert!(matches!(error, ents_forge::Error::InvalidArgument(_)));

    let queued = fixture.queued_session();
    fixture.claim(&queued).expect("claims");
    let error = agent::reopen(
        &fixture.refs,
        &fixture.objects,
        &NullEventSink,
        &queued,
        &fixture.identity(),
        Mode::Advisory,
    )
    .expect_err("refused: running, past the point of no return");
    assert!(matches!(error, ents_forge::Error::InvalidArgument(_)));
}

/// `append_thread` appends a chat turn while `planning`, touching nothing
/// else.
// @relation(scope=function, role=Verifies)
#[rstest]
fn append_thread_appends_a_turn_while_planning() {
    let fixture = Fixture::new();
    let id = fixture.new_session();
    let before = agent::show(&fixture.refs, &fixture.objects, &id).expect("shows");
    let turns_before = before.thread.len();

    let outcome = agent::append_thread(
        &fixture.refs,
        &fixture.objects,
        &NullEventSink,
        &id,
        vec![b"user: what should the plan look like?".to_vec()],
        &fixture.identity(),
        Mode::Advisory,
    )
    .expect("appends");
    assert_eq!(outcome.result, TxResult::Applied);

    let session = agent::show(&fixture.refs, &fixture.objects, &id).expect("shows");
    assert_eq!(session.meta.status, Status::Planning);
    assert_eq!(session.thread.len(), turns_before.saturating_add(1));
}

/// `append_thread` also accepts a turn while `ready`-and-awaiting
/// confirmation, but refuses once the session is queued, running, or
/// terminal — `docs/agent-sessions-plan.adoc`'s Phase 4 acceptance: "after
/// confirm, no endpoint accepts messages or revisions without the explicit
/// un-queue."
// @relation(scope=function, role=Verifies)
#[rstest]
fn append_thread_refuses_a_queued_running_or_terminal_session() {
    let fixture = Fixture::new();

    let awaiting = fixture.new_session();
    fixture.revise_plan(&awaiting, "do the thing");
    agent::append_thread(
        &fixture.refs,
        &fixture.objects,
        &NullEventSink,
        &awaiting,
        vec![b"one more thought".to_vec()],
        &fixture.identity(),
        Mode::Advisory,
    )
    .expect("awaiting confirmation still accepts a chat turn");

    let queued = fixture.queued_session();
    let error = agent::append_thread(
        &fixture.refs,
        &fixture.objects,
        &NullEventSink,
        &queued,
        vec![b"a message that must not land".to_vec()],
        &fixture.identity(),
        Mode::Advisory,
    )
    .expect_err("refused: queued");
    assert!(matches!(error, ents_forge::Error::InvalidArgument(_)));

    fixture.claim(&queued).expect("claims");
    let error = agent::append_thread(
        &fixture.refs,
        &fixture.objects,
        &NullEventSink,
        &queued,
        vec![b"a message that must not land".to_vec()],
        &fixture.identity(),
        Mode::Advisory,
    )
    .expect_err("refused: running");
    assert!(matches!(error, ents_forge::Error::InvalidArgument(_)));
}

/// `draft_plan` commits a plan and transcript in one commit, transitioning
/// `planning` to `ready` — the `agent-plan` effect's own commit.
// @relation(scope=function, role=Verifies)
#[rstest]
fn draft_plan_commits_the_plan_and_transcript_and_transitions_to_ready() {
    let fixture = Fixture::new();
    let id = fixture.new_session();

    let outcome = agent::draft_plan(
        &fixture.refs,
        &fixture.objects,
        &NullEventSink,
        &id,
        "1. read the failing test\n2. fix it\n3. re-run".to_owned(),
        vec![b"drafting transcript".to_vec()],
        &fixture.identity(),
        Mode::Advisory,
    )
    .expect("drafts");
    assert_eq!(outcome.result, TxResult::Applied);

    let session = agent::show(&fixture.refs, &fixture.objects, &id).expect("shows");
    assert_eq!(session.meta.status, Status::Ready);
    assert!(session.awaiting_confirmation());
    assert_eq!(
        session.plan.as_deref(),
        Some("1. read the failing test\n2. fix it\n3. re-run")
    );
    assert_eq!(
        session.thread.last().map(Vec::as_slice),
        Some(b"drafting transcript".as_slice())
    );
}

/// `draft_plan` refuses a session that is not `planning` — a human already
/// moved it to `ready` (or further) by hand, and this function must not
/// clobber that race rather than silently overwrite it.
// @relation(scope=function, role=Verifies)
#[rstest]
fn draft_plan_refuses_a_session_that_is_not_planning() {
    let fixture = Fixture::new();
    let id = fixture.new_session();
    fixture.revise_plan(&id, "a human already drafted this");

    let error = agent::draft_plan(
        &fixture.refs,
        &fixture.objects,
        &NullEventSink,
        &id,
        "a race draft".to_owned(),
        vec![],
        &fixture.identity(),
        Mode::Advisory,
    )
    .expect_err("refused: already ready");
    assert!(matches!(error, ents_forge::Error::InvalidArgument(_)));
}
