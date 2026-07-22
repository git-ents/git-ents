//! The `agent-plan` effect's run path (`docs/agent-sessions-plan.adoc`'s
//! Phase 4): headless plan drafting for a `planning` session that carries a
//! prompt and no plan yet, landing the drafted plan and its own results
//! record atomically.
//!
//! # Why this lives here, not in `ents-effect` or `ents-forge`
//!
//! Exactly [`crate::agent_worker`]'s own reasoning: this needs both
//! [`ents_forge::agent`]'s typed session commands and
//! [`ents_effect::Executor`]'s sandbox seam at once, which neither kernel
//! crate may depend on the other to provide (`ents-effect`'s `Cargo.toml`
//! links exactly `ents-model`, `ents-query`, and `ents-receive`; `ents-forge`
//! is not among them, by design, from both sides — see
//! `ents_forge::agent::dispatch`'s own doc). `git-ents` already depends on
//! both, so this is the same "session handler" composition-root seam
//! [`crate::hook::post_receive`] installs [`crate::agent_worker`] for,
//! installed here for the one other effect name
//! ([`AGENT_PLAN_NAME`]) that needs bespoke handling.
//!
//! # No claim, unlike `agent-exec`
//!
//! Drafting a plan is read-only context gathering against the declared base
//! ref, never a mutation of anything but the session's own `plan`/`thread` —
//! there is nothing here for a claim to protect (`docs/agent-sessions-plan.adoc`'s
//! Phase 4: "claim is NOT needed here"). Two workers racing to draft the
//! same session concurrently are serialized by `receive`'s own compare-
//! and-swap on the session ref (`receive.refstore-seam`'s atomic write
//! step): the loser's [`ents_forge::agent::draft_plan_transition`] proposal
//! comes back as anything but [`TxResult::Applied`], and this module
//! surfaces that exactly like [`crate::agent_worker::run_agent_exec`]'s own
//! lost-claim race — a cheap `pass` recorded to discharge the obligation,
//! no error, no retry of this same dequeued oid.
//!
//! # How the prompt reaches the sandbox, and the draft comes back
//!
//! Mirrors [`crate::agent_worker`]'s own sideband-file convention: the
//! session's seeded prompt (`AgentSession::thread`'s first turn) is written
//! to [`AGENT_PROMPT_FILE`] in the checked-out workdir before
//! [`ents_effect::Executor::run`] is called; the declared command is
//! expected to write its drafted plan text to [`AGENT_PLAN_DRAFT_FILE`] in
//! that same workdir, read back once the command completes. Unlike
//! `agent-exec`, no output tree is ever captured from the workdir — plan
//! drafting is read-only context gathering, not a code change, so there is
//! no result branch to push and nothing to clean up before an object write
//! that never happens.

use std::path::{Path, PathBuf};

use ents_effect::executor::SandboxInputs;
use ents_effect::run::short_oid;
use ents_effect::{Executor, RunStatus};
use ents_forge::agent::{AgentSession, PlanDispatch, dispatch_plan};
use ents_model::{ResultRecord, Status as ResultStatus};
use ents_receive::{EventSink, Identity, Mode, Outcome, Proposal, TxResult};
use gix_hash::ObjectId;
use gix_object::{CommitRef, Find, Kind, Write};
use gix_ref_store::RefStore;

use crate::commands::commit_tree;
use crate::error::{Error, Result};

/// `agent-plan`'s own effect name — re-exported so a caller deciding which
/// bespoke handler a dequeued `(effect, oid)` obligation belongs to does
/// not need a second import of `ents_effect::definition`.
pub const AGENT_PLAN_NAME: &str = ents_effect::definition::AGENT_PLAN_NAME;

/// The file the session's seeded prompt is written to inside the
/// checked-out workdir before the drafting command runs (see this module's
/// own doc).
pub const AGENT_PROMPT_FILE: &str = ".git-ents-agent-prompt.txt";

/// The file the drafting command is expected to write its drafted plan
/// text to, read back once it completes.
pub const AGENT_PLAN_DRAFT_FILE: &str = ".git-ents-agent-plan-draft.txt";

/// What running the `agent-plan` effect against one dequeued `(effect,
/// oid)` obligation did.
#[derive(Debug)]
pub enum AgentPlanOutcome {
    /// The tip was not a `planning` session with a prompt and no plan; a
    /// cheap `pass` was recorded to discharge the obligation, no session
    /// was touched.
    NoOp,
    /// The drafting command ran and reported failure (an ordinary,
    /// completed result, not an infrastructure failure): a `fail` was
    /// recorded on this effect's own results ref, and the session was left
    /// untouched, still `planning` and still dispatchable on a future
    /// commit.
    DraftFailed,
    /// This worker drafted a plan, but by the time its finalize proposal
    /// reached `receive`, another worker's own draft had already landed
    /// (`receive`'s CAS on the session ref serialized the race) — a `pass`
    /// was recorded for the dequeued oid instead, and this worker's own
    /// draft was discarded.
    DraftLost,
    /// This worker drafted the plan and landed it, atomically with this
    /// effect's own results record.
    Drafted {
        /// The session's own genesis-oid id.
        id: String,
        /// The atomic finalize's outcome.
        outcome: Outcome,
    },
}

/// Run the `agent-plan` effect against the single dequeued commit `oid` —
/// a tip entering `refs/meta/agent-sessions/*`
/// (`ents_effect::definition::AGENT_PLAN_TRIGGER`'s own `meta()` semantics).
///
/// `toolchains` and `command` are this effect's own declared toolchains
/// (already resolved to host `bin/` directories) and its declared `run`
/// command; `scratch` is where the base tree is checked out for one run,
/// mirroring [`crate::agent_worker::run_agent_exec`]'s identical
/// parameters.
///
/// # Errors
///
/// Any [`Error`] from reading or decoding the session, checking out the
/// base tree, [`Executor::run`] itself (an infrastructure failure
/// propagates with nothing published: the session stays `planning`
/// untouched, and the queue's own retry policy is what revisits it,
/// `effect.result-taxonomy`), reading back the drafted plan file, or
/// building and sending the finalize proposal.
// @relation(effect.execution, effect.results-writeback, effect.result-taxonomy, receive.multi-ref-atomicity, scope=function)
#[expect(
    clippy::too_many_arguments,
    reason = "one input per materialization/identity step, mirrors run_agent_exec's own shape"
)]
pub fn run_agent_plan<O>(
    refs: &dyn RefStore,
    objects: &O,
    events: &dyn EventSink,
    executor: &dyn Executor,
    scratch: &Path,
    toolchains: &[(String, PathBuf)],
    command: &str,
    oid: ObjectId,
    author: &gix::actor::Signature,
    sign: &dyn Fn(&[u8]) -> String,
    mode: Mode,
) -> Result<AgentPlanOutcome>
where
    O: Find + Write,
{
    let tree = commit_tree(objects, oid)?;
    let session: AgentSession = facet_git_tree::deserialize(&tree, objects)?;
    let results_ref = ents_model::namespace::result_ref(AGENT_PLAN_NAME, &short_oid(oid))?;

    if dispatch_plan(&session) == PlanDispatch::NoOp {
        record_result(
            refs,
            objects,
            events,
            &results_ref,
            oid,
            ResultStatus::Pass,
            author,
            sign,
            mode,
        )?;
        return Ok(AgentPlanOutcome::NoOp);
    }

    let id = genesis_of(objects, oid)?.to_string();
    let identity = Identity {
        actor: author.clone(),
        author: None,
        sign,
    };

    // Read-only context gathering against the declared base ref: no output
    // tree is ever captured back from this checkout (unlike `agent-exec`'s
    // run), since drafting a plan is never a code change.
    let base_ref: gix::refs::FullName =
        session
            .meta
            .base_ref
            .clone()
            .try_into()
            .map_err(|_source| {
                Error::InvalidArgument(format!(
                    "agent session {id}'s base ref {:?} is not a well-formed refname",
                    session.meta.base_ref
                ))
            })?;
    let base_tip = refs
        .get(base_ref.as_ref())?
        .ok_or_else(|| Error::NotFound {
            what: session.meta.base_ref.clone(),
        })?;
    let base_tree = commit_tree(objects, base_tip)?;

    let workdir = scratch.join(oid.to_string());
    reset_dir(&workdir)?;
    ents_effect::materialize::checkout(objects, base_tree, &workdir)?;

    let prompt = session
        .thread
        .first()
        .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        .unwrap_or_default();
    let prompt_path = workdir.join(AGENT_PROMPT_FILE);
    std::fs::write(&prompt_path, &prompt).map_err(|source| Error::Io {
        path: prompt_path.clone(),
        source,
    })?;

    let inputs = SandboxInputs {
        workdir: &workdir,
        toolchains,
        command,
    };
    // An `Err` here is an infrastructure failure the sandbox itself never
    // turned into a completed pass/fail: it propagates as-is, past every
    // write below, so the finalize proposal is never even built — the
    // session stays `planning` (a legal continuation), the queue's own
    // retry bound decides what happens next.
    let output = executor.run(&inputs)?;

    if output.status == RunStatus::Fail {
        // A completed, failed drafting attempt: record it and leave the
        // session untouched (still `planning`, still no plan) rather than
        // ever moving it toward a terminal `Status::Failed` — that variant
        // names a session's own run failing, not a drafting attempt that
        // may simply be retried on a future commit.
        record_result(
            refs,
            objects,
            events,
            &results_ref,
            oid,
            ResultStatus::Fail,
            author,
            sign,
            mode,
        )?;
        return Ok(AgentPlanOutcome::DraftFailed);
    }

    let draft_path = workdir.join(AGENT_PLAN_DRAFT_FILE);
    let plan_text = std::fs::read_to_string(&draft_path).map_err(|source| Error::Io {
        path: draft_path.clone(),
        source,
    })?;

    let (draft_transition, draft_tip) = match ents_forge::agent::draft_plan_transition(
        refs,
        objects,
        &id,
        plan_text,
        vec![output.log.into_bytes()],
        &identity,
    ) {
        Ok(built) => built,
        // The ordinary "first drafter wins, losers no-op" race: by the
        // time this worker re-read the session fresh to build its own
        // transition, another worker's draft had already landed, so the
        // session is no longer `Planning`
        // (`ents_forge::agent::draft_plan_transition`'s own precondition).
        Err(ents_forge::Error::InvalidArgument(_)) => {
            record_result(
                refs,
                objects,
                events,
                &results_ref,
                oid,
                ResultStatus::Pass,
                author,
                sign,
                mode,
            )?;
            return Ok(AgentPlanOutcome::DraftLost);
        }
        Err(other) => return Err(other.into()),
    };
    let record = ResultRecord::new(AGENT_PLAN_NAME, oid, ResultStatus::Pass);
    let (result_transition, result_tip) = ents_receive::entity_transition(
        refs,
        objects,
        &results_ref,
        &record,
        &identity,
        "Record agent-plan result",
    )?;

    // Finalize = one atomic multi-ref proposal: the session's draft and
    // this effect's own result land together or not at all
    // (`receive.multi-ref-atomicity`).
    let proposal = Proposal {
        transitions: vec![draft_transition, result_transition],
        objects: vec![draft_tip, result_tip],
        auth: None,
    };
    let outcome = ents_receive::receive(refs, objects, events, &proposal, mode)?;
    if outcome.result != TxResult::Applied {
        // Lost the race: another worker's draft already landed between
        // this worker's own read and its finalize attempt — `receive`'s
        // CAS on the session ref is what serializes concurrent drafters
        // (`docs/agent-sessions-plan.adoc`'s Phase 4). Record a cheap pass
        // to discharge this obligation, exactly like
        // `agent_worker::run_agent_exec`'s own lost-claim race.
        record_result(
            refs,
            objects,
            events,
            &results_ref,
            oid,
            ResultStatus::Pass,
            author,
            sign,
            mode,
        )?;
        return Ok(AgentPlanOutcome::DraftLost);
    }
    Ok(AgentPlanOutcome::Drafted { id, outcome })
}

/// Record a result for `oid` on the canonical `agent-plan` results ref —
/// the no-op, failed-draft, and lost-race paths all discharge their
/// obligation through this one call.
#[expect(
    clippy::too_many_arguments,
    reason = "one input per write_result parameter; a thin, single-call wrapper"
)]
fn record_result<O: Find + Write>(
    refs: &dyn RefStore,
    objects: &O,
    events: &dyn EventSink,
    results_ref: &gix::refs::FullName,
    oid: ObjectId,
    status: ResultStatus,
    author: &gix::actor::Signature,
    sign: &dyn Fn(&[u8]) -> String,
    mode: Mode,
) -> Result<Outcome> {
    Ok(ents_effect::write_result(
        refs,
        objects,
        events,
        results_ref.clone(),
        AGENT_PLAN_NAME,
        oid,
        status,
        author,
        sign,
        mode,
    )?)
}

/// Wipe and recreate `dir` — a fresh workdir per run; mirrors
/// `agent_worker`'s identical helper.
fn reset_dir(dir: &Path) -> Result<()> {
    if dir.exists() {
        std::fs::remove_dir_all(dir).map_err(|source| Error::Io {
            path: dir.to_owned(),
            source,
        })?;
    }
    std::fs::create_dir_all(dir).map_err(|source| Error::Io {
        path: dir.to_owned(),
        source,
    })
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

    use ents_forge::agent::ReviewPolicy;
    use ents_gate::Config;
    use ents_model::{MemberId, Provenance, namespace};
    use ents_receive::NullEventSink;
    use ents_testutil::{Keypair, MemRefStore, ObjectStore, advance_ref, enroll_member};
    use gix_ref_store::RefStoreRead as _;
    use rstest::rstest;

    use super::*;

    struct StubExecutor {
        status: RunStatus,
        prepare: fn(&Path),
    }

    impl Executor for StubExecutor {
        fn run(&self, inputs: &SandboxInputs<'_>) -> ents_effect::Result<ents_effect::RunOutput> {
            (self.prepare)(inputs.workdir);
            Ok(ents_effect::RunOutput {
                status: self.status,
                log: "planning transcript".to_owned(),
            })
        }
    }

    fn no_draft(workdir: &Path) {
        assert!(
            workdir.join(AGENT_PROMPT_FILE).exists(),
            "the prompt sideband file must exist before the drafting command runs"
        );
    }

    fn writes_a_draft(workdir: &Path) {
        no_draft(workdir);
        std::fs::write(
            workdir.join(AGENT_PLAN_DRAFT_FILE),
            "1. read the test\n2. fix it\n3. re-run",
        )
        .expect("write draft");
    }

    fn writes_a_different_draft(workdir: &Path) {
        no_draft(workdir);
        std::fs::write(
            workdir.join(AGENT_PLAN_DRAFT_FILE),
            "this worker's own draft",
        )
        .expect("write draft");
    }

    struct FailingExecutor;
    impl Executor for FailingExecutor {
        fn run(&self, _inputs: &SandboxInputs<'_>) -> ents_effect::Result<ents_effect::RunOutput> {
            Err(ents_effect::Error::Sandbox(
                "the sandbox never started".to_owned(),
            ))
        }
    }

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

        /// A brand-new, `planning` session with a prompt and no plan — the
        /// only precondition [`super::run_agent_plan`]'s
        /// [`PlanDispatch::Draft`] arm runs against.
        fn planning_session(&self) -> (ObjectId, String) {
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
                    review_policy: ReviewPolicy::Manual,
                    retry_of: None,
                },
                &identity,
                Mode::Advisory,
            )
            .expect("creates");
            assert_eq!(outcome.result, TxResult::Applied);

            let ref_name = ents_model::namespace::agent_session_ref(&id).expect("valid");
            let tip = self
                .refs
                .get(ref_name.as_ref())
                .expect("readable")
                .expect("exists");
            (tip, id)
        }
    }

    #[rstest]
    // @relation(scope=function, role=Verifies)
    fn a_ready_session_is_a_cheap_no_op() {
        let fixture = Fixture::new();
        let (oid, id) = fixture.planning_session();
        // Move it to `ready` by hand, as a human redrafting would.
        ents_forge::agent::revise_plan(
            &fixture.refs,
            &fixture.objects,
            &NullEventSink,
            &id,
            "already drafted".to_owned(),
            &fixture.identity(),
            Mode::Advisory,
        )
        .expect("revises");
        let ref_name = ents_model::namespace::agent_session_ref(&id).expect("valid");
        let ready_oid = fixture
            .refs
            .get(ref_name.as_ref())
            .expect("readable")
            .expect("exists");

        let author = fixture.author();
        let run = run_agent_plan(
            &fixture.refs,
            &fixture.objects,
            &NullEventSink,
            &FailingExecutor,
            Path::new("/does/not/matter"),
            &[],
            "true",
            ready_oid,
            &author,
            fixture.sign_fn(),
            Mode::Advisory,
        )
        .expect("dispatch never touches the sandbox for a no-op");
        assert!(matches!(run, AgentPlanOutcome::NoOp));

        let results_ref = ents_model::namespace::result_ref(AGENT_PLAN_NAME, &short_oid(ready_oid))
            .expect("valid");
        assert!(
            fixture
                .refs
                .get(results_ref.as_ref())
                .expect("readable")
                .is_some(),
            "the no-op path must still discharge the obligation with a recorded pass"
        );
        // Sanity: the originally dequeued `planning` oid is untouched by
        // this assertion path — `oid` above was reassigned to the later
        // `ready` tip deliberately, since dispatch always re-reads the
        // *current* tip's own decoded state, never the dequeued oid's
        // historical one.
        let _ = oid;
    }

    #[rstest]
    // @relation(effect.execution, effect.results-writeback, receive.multi-ref-atomicity, scope=function, role=Verifies)
    fn a_planning_session_with_a_prompt_drafts_and_lands_ready() {
        let fixture = Fixture::new();
        let (oid, id) = fixture.planning_session();

        let scratch = tempfile::tempdir().expect("tempdir");
        let executor = StubExecutor {
            status: RunStatus::Pass,
            prepare: writes_a_draft,
        };
        let author = fixture.author();
        let run = run_agent_plan(
            &fixture.refs,
            &fixture.objects,
            &NullEventSink,
            &executor,
            scratch.path(),
            &[],
            "true",
            oid,
            &author,
            fixture.sign_fn(),
            Mode::Advisory,
        )
        .expect("drafts and finalizes");
        let AgentPlanOutcome::Drafted {
            id: drafted_id,
            outcome,
        } = run
        else {
            panic!("expected Drafted, got {run:?}");
        };
        assert_eq!(drafted_id, id);
        assert_eq!(outcome.result, TxResult::Applied);

        let session = ents_forge::agent::show(&fixture.refs, &fixture.objects, &id).expect("shows");
        assert_eq!(session.meta.status, ents_forge::agent::Status::Ready);
        assert!(session.awaiting_confirmation());
        assert_eq!(
            session.plan.as_deref(),
            Some("1. read the test\n2. fix it\n3. re-run")
        );
        assert!(
            !session.thread.is_empty(),
            "the drafting run's transcript must land as a thread blob"
        );

        let results_ref =
            ents_model::namespace::result_ref(AGENT_PLAN_NAME, &short_oid(oid)).expect("valid");
        let result_tip = fixture
            .refs
            .get(results_ref.as_ref())
            .expect("readable")
            .expect("result landed");
        let result_tree = commit_tree(&fixture.objects, result_tip).expect("tree");
        let record: ResultRecord =
            facet_git_tree::deserialize(&result_tree, &fixture.objects).expect("decodes");
        assert_eq!(record.status, ResultStatus::Pass);
        assert_eq!(record.target(), oid);
    }

    #[rstest]
    // @relation(effect.result-taxonomy, scope=function, role=Verifies)
    fn a_failed_draft_records_a_fail_result_and_leaves_the_session_planning() {
        let fixture = Fixture::new();
        let (oid, id) = fixture.planning_session();

        let scratch = tempfile::tempdir().expect("tempdir");
        let executor = StubExecutor {
            status: RunStatus::Fail,
            prepare: no_draft,
        };
        let author = fixture.author();
        let run = run_agent_plan(
            &fixture.refs,
            &fixture.objects,
            &NullEventSink,
            &executor,
            scratch.path(),
            &[],
            "true",
            oid,
            &author,
            fixture.sign_fn(),
            Mode::Advisory,
        )
        .expect("runs");
        assert!(matches!(run, AgentPlanOutcome::DraftFailed));

        let session = ents_forge::agent::show(&fixture.refs, &fixture.objects, &id).expect("shows");
        assert_eq!(session.meta.status, ents_forge::agent::Status::Planning);
        assert!(session.plan.is_none());

        let results_ref =
            ents_model::namespace::result_ref(AGENT_PLAN_NAME, &short_oid(oid)).expect("valid");
        let result_tip = fixture
            .refs
            .get(results_ref.as_ref())
            .expect("readable")
            .expect("result landed");
        let result_tree = commit_tree(&fixture.objects, result_tip).expect("tree");
        let record: ResultRecord =
            facet_git_tree::deserialize(&result_tree, &fixture.objects).expect("decodes");
        assert_eq!(record.status, ResultStatus::Fail);
    }

    #[rstest]
    // @relation(effect.result-taxonomy, scope=function, role=Verifies)
    fn a_sandbox_infrastructure_failure_publishes_nothing_and_leaves_the_session_planning() {
        let fixture = Fixture::new();
        let (oid, id) = fixture.planning_session();

        let scratch = tempfile::tempdir().expect("tempdir");
        let author = fixture.author();
        let error = run_agent_plan(
            &fixture.refs,
            &fixture.objects,
            &NullEventSink,
            &FailingExecutor,
            scratch.path(),
            &[],
            "true",
            oid,
            &author,
            fixture.sign_fn(),
            Mode::Advisory,
        )
        .expect_err("an infra failure must propagate, not silently finalize");
        assert!(matches!(error, Error::Effect(_)));

        let session = ents_forge::agent::show(&fixture.refs, &fixture.objects, &id).expect("shows");
        assert_eq!(session.meta.status, ents_forge::agent::Status::Planning);
        assert!(session.plan.is_none());

        let results_ref =
            ents_model::namespace::result_ref(AGENT_PLAN_NAME, &short_oid(oid)).expect("valid");
        assert!(
            fixture
                .refs
                .get(results_ref.as_ref())
                .expect("readable")
                .is_none(),
            "no result may be recorded when the sandbox never completed the run"
        );
    }

    #[rstest]
    // @relation(scope=function, role=Verifies)
    fn a_lost_draft_race_records_a_pass_and_leaves_the_racing_draft_alone() {
        let fixture = Fixture::new();
        let (oid, id) = fixture.planning_session();

        // Simulate a second worker having already drafted the session
        // before this worker's own finalize attempt runs.
        ents_forge::agent::draft_plan(
            &fixture.refs,
            &fixture.objects,
            &NullEventSink,
            &id,
            "someone else's draft".to_owned(),
            vec![],
            &fixture.identity(),
            Mode::Advisory,
        )
        .expect("first draft succeeds");

        let scratch = tempfile::tempdir().expect("tempdir");
        let executor = StubExecutor {
            status: RunStatus::Pass,
            prepare: writes_a_different_draft,
        };
        let author = fixture.author();
        let run = run_agent_plan(
            &fixture.refs,
            &fixture.objects,
            &NullEventSink,
            &executor,
            scratch.path(),
            &[],
            "true",
            oid,
            &author,
            fixture.sign_fn(),
            Mode::Advisory,
        )
        .expect("a lost race is not an error");
        assert!(matches!(run, AgentPlanOutcome::DraftLost));

        let session = ents_forge::agent::show(&fixture.refs, &fixture.objects, &id).expect("shows");
        assert_eq!(
            session.plan.as_deref(),
            Some("someone else's draft"),
            "the losing worker's own draft must never land on the session"
        );
    }
}
