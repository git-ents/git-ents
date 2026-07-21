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

use ents_forge::agent::{self, FailureReason, NewAgentSession, ReviewPolicy, Status};
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
