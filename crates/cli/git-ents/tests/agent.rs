//! Integration coverage for `git ents agent` against a real local
//! composition root (`roots.local`): the plan-and-confirm ceremony
//! (`new`, `plan`, `confirm`) end to end, `list`/`show` reading it back,
//! and the guard that refuses confirming a session with no plan yet —
//! mirroring `tests/issue.rs`'s own shape for the same reason: every
//! operation here is `commands::agent`'s own library call
//! (`lens.parity`), not a re-implementation for the test.
//!
//! The mobile end-to-end test at the bottom (`docs/agent-sessions-plan.adoc`'s
//! Phase 4 acceptance) additionally drives `git_ents::plan_worker` and
//! `git_ents::agent_worker` directly — the same composition-root run
//! functions `crate::hook::post_receive` calls for the `agent-plan` and
//! `agent-exec` effects, exercised here without a real push/hook cycle.

#![allow(clippy::expect_used, reason = "integration test")]

mod common;

use ents_effect::executor::SandboxInputs;
use ents_effect::{Executor, RunOutput, RunStatus};
use ents_forge::agent::Status;
use ents_model::MemberId;
use ents_receive::Mode;
use git_ents::commands::agent;
use git_ents::root::LocalRoot;
use git_ents::{agent_worker, plan_worker};
use gix_object::{Commit, Kind};
use gix_ref_store::{Expected, RefEdit, RefStore, RefStoreRead as _};

/// `git ents agent new` seeds `planning`, with no plan yet — neither
/// derived predicate holds.
// @relation(model.extensibility, scope=function, role=Verifies)
#[test]
fn agent_new_starts_in_planning_with_no_plan() {
    let fixture = common::Fixture::new(1);
    let root = LocalRoot::open(fixture.path()).expect("opens");

    let id = agent::new(
        &root,
        "fix the flaky test".to_owned(),
        "claude-sonnet-5".to_owned(),
        vec![],
        "refs/heads/main".to_owned(),
        "manual".to_owned(),
        None,
        Some(fixture.key_path.clone()),
    )
    .expect("starts a session");

    let session = agent::show(&root, &id).expect("shows");
    assert_eq!(session.meta.status, Status::Planning);
    assert!(session.plan.is_none());
    assert!(!session.queued());
    assert!(!session.awaiting_confirmation());
}

/// The full plan-and-confirm ceremony: `plan` drafts the text and moves the
/// session to `ready`/awaiting-confirmation; `confirm` binds the plan hash,
/// moving it to `ready`/queued — the only state
/// `docs/agent-sessions-plan.adoc`'s Phase 2 worker ever claims out of.
// @relation(model.extensibility, scope=function, role=Verifies)
#[test]
fn agent_plan_then_confirm_reaches_queued() {
    let fixture = common::Fixture::new(2);
    let root = LocalRoot::open(fixture.path()).expect("opens");

    let id = agent::new(
        &root,
        "fix the flaky test".to_owned(),
        "claude-sonnet-5".to_owned(),
        vec![],
        "refs/heads/main".to_owned(),
        "manual".to_owned(),
        None,
        Some(fixture.key_path.clone()),
    )
    .expect("starts a session");

    agent::plan(
        &root,
        &id,
        "read the flaky test, find the race, fix it".to_owned(),
        Some(fixture.key_path.clone()),
    )
    .expect("drafts a plan");

    let drafted = agent::show(&root, &id).expect("shows");
    assert_eq!(drafted.meta.status, Status::Ready);
    assert!(drafted.awaiting_confirmation());
    assert!(!drafted.queued());

    agent::confirm(&root, &id, None, Some(fixture.key_path.clone())).expect("confirms");

    let queued = agent::show(&root, &id).expect("shows");
    assert!(queued.queued());
    assert!(!queued.awaiting_confirmation());
}

/// Revising the plan after a confirm drops the stale confirm, returning the
/// session to awaiting confirmation — the CLI surface for the same
/// guarantee `ents-forge`'s own `agent_sessions.rs` integration test proves
/// at the command layer directly.
// @relation(scope=function, role=Verifies)
#[test]
fn agent_revising_the_plan_drops_a_stale_confirm() {
    let fixture = common::Fixture::new(3);
    let root = LocalRoot::open(fixture.path()).expect("opens");

    let id = agent::new(
        &root,
        "prompt".to_owned(),
        "claude-sonnet-5".to_owned(),
        vec![],
        "refs/heads/main".to_owned(),
        "manual".to_owned(),
        None,
        Some(fixture.key_path.clone()),
    )
    .expect("starts a session");
    agent::plan(
        &root,
        &id,
        "first draft".to_owned(),
        Some(fixture.key_path.clone()),
    )
    .expect("drafts");
    agent::confirm(&root, &id, None, Some(fixture.key_path.clone())).expect("confirms");
    assert!(agent::show(&root, &id).expect("shows").queued());

    agent::plan(
        &root,
        &id,
        "a materially different plan".to_owned(),
        Some(fixture.key_path.clone()),
    )
    .expect("redrafts");

    let revised = agent::show(&root, &id).expect("shows");
    assert!(revised.confirm.is_none());
    assert!(revised.awaiting_confirmation());
    assert!(!revised.queued());
}

/// `confirm` refuses a session with no plan yet — the CLI surfaces the
/// command layer's own guard as an ordinary error, not a panic.
// @relation(scope=function, role=Verifies)
#[test]
fn agent_confirm_refuses_a_session_with_no_plan() {
    let fixture = common::Fixture::new(4);
    let root = LocalRoot::open(fixture.path()).expect("opens");

    let id = agent::new(
        &root,
        "prompt".to_owned(),
        "claude-sonnet-5".to_owned(),
        vec![],
        "refs/heads/main".to_owned(),
        "manual".to_owned(),
        None,
        Some(fixture.key_path.clone()),
    )
    .expect("starts a session");

    let error = agent::confirm(&root, &id, None, Some(fixture.key_path.clone()))
        .expect_err("refused: no plan to confirm");
    assert!(matches!(error, git_ents::Error::Forge(_)));
}

/// `git ents agent list` reports every session recorded, including one
/// still in `planning`.
// @relation(scope=function, role=Verifies)
#[test]
fn agent_list_reports_every_session() {
    let fixture = common::Fixture::new(5);
    let root = LocalRoot::open(fixture.path()).expect("opens");

    let first = agent::new(
        &root,
        "first prompt".to_owned(),
        "claude-sonnet-5".to_owned(),
        vec![],
        "refs/heads/main".to_owned(),
        "manual".to_owned(),
        None,
        Some(fixture.key_path.clone()),
    )
    .expect("starts a session");
    let second = agent::new(
        &root,
        "second prompt".to_owned(),
        "claude-sonnet-5".to_owned(),
        vec![],
        "refs/heads/main".to_owned(),
        "auto".to_owned(),
        None,
        Some(fixture.key_path.clone()),
    )
    .expect("starts a session");

    let ids: Vec<String> = agent::list(&root)
        .expect("lists")
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert!(ids.contains(&first));
    assert!(ids.contains(&second));
}

// ---------------------------------------------------------------------
// Mobile end-to-end (`docs/agent-sessions-plan.adoc`'s Phase 4
// acceptance): prompt in -> headless draft -> confirm from a second
// request -> execution.
// ---------------------------------------------------------------------

/// Write an empty-tree commit and move `refname` to it directly through
/// the ref store — a branch ref needs no signature at all
/// (`gate.principled-split`), mirroring `tests/reconcile.rs`'s own
/// `advance_branch` but generalized to any `RefStore`/object-store pair
/// rather than one root type, since this file's fixture is a
/// [`LocalRoot`], not a `HostedRoot`.
fn advance_branch(
    refs: &dyn RefStore,
    objects: &impl gix_object::Write,
    refname: &str,
    seconds: i64,
) -> gix_hash::ObjectId {
    let empty_tree = objects.write(&gix_object::Tree::empty()).expect("tree");
    let actor = gix::actor::Signature {
        name: "test".into(),
        email: "test@ents.test".into(),
        time: gix::date::Time { seconds, offset: 0 },
    };
    let commit = Commit {
        tree: empty_tree,
        parents: Default::default(),
        author: actor.clone(),
        committer: actor,
        encoding: None,
        message: "advance".into(),
        extra_headers: Vec::new(),
    };
    let mut raw = Vec::new();
    gix_object::WriteTo::write_to(&commit, &mut raw).expect("serialize");
    let oid = objects.write_buf(Kind::Commit, &raw).expect("write");

    let name: gix::refs::FullName = refname.try_into().expect("valid refname");
    refs.transaction(&[RefEdit {
        name,
        expected: Expected::Any,
        new: Some(oid),
    }])
    .expect("moves the ref");
    oid
}

/// A stub `agent-plan` executor: writes a drafted plan file to the
/// checked-out workdir, exactly the contract
/// `git_ents::plan_worker::run_agent_plan`'s own doc describes for the
/// declared `run` command.
fn write_draft(workdir: &std::path::Path) {
    std::fs::write(
        workdir.join(plan_worker::AGENT_PLAN_DRAFT_FILE),
        "1. reproduce the flake\n2. fix the race\n3. verify it stays green",
    )
    .expect("write draft");
}

struct DraftExecutor;
impl Executor for DraftExecutor {
    fn run(&self, inputs: &SandboxInputs<'_>) -> ents_effect::Result<RunOutput> {
        write_draft(inputs.workdir);
        Ok(RunOutput {
            status: RunStatus::Pass,
            log: "drafted".to_owned(),
        })
    }
}

/// A stub `agent-exec` executor: no filesystem mutation, always passes.
struct ExecExecutor;
impl Executor for ExecExecutor {
    fn run(&self, _inputs: &SandboxInputs<'_>) -> ents_effect::Result<RunOutput> {
        Ok(RunOutput {
            status: RunStatus::Pass,
            log: "executed".to_owned(),
        })
    }
}

/// The full mobile path: a session created with a prompt only (no plan)
/// drafts headlessly through the `agent-plan` effect (a stubbed executor
/// standing in for the real headless agent SDK), reaches `ready` with a
/// plan, is confirmed from what stands in here for "a second request" (a
/// fresh, independent `agent::confirm` call against the same repository —
/// the same call shape a second HTTP request to the hosted web UI would
/// make), reaches `queued`, and the `agent-exec` effect's own claim
/// succeeds against it and runs it to `done` — reusing
/// `git_ents::plan_worker::run_agent_plan` and
/// `git_ents::agent_worker::run_agent_exec` directly, the same
/// composition-root seam `crate::hook::post_receive` drives for both
/// effects.
// @relation(scope=function, role=Verifies)
#[test]
fn mobile_end_to_end_prompt_to_headless_draft_to_confirm_to_claim() {
    let fixture = common::Fixture::new(6);
    let root = LocalRoot::open(fixture.path()).expect("opens");
    advance_branch(&root.refs, &root.objects, "refs/heads/main", 100);

    // 1. A session created with a prompt only -- no plan.
    let id = agent::new(
        &root,
        "fix the flaky test".to_owned(),
        "claude-sonnet-5".to_owned(),
        vec![],
        "refs/heads/main".to_owned(),
        "manual".to_owned(),
        None,
        Some(fixture.key_path.clone()),
    )
    .expect("starts a session");
    let planning = agent::show(&root, &id).expect("shows");
    assert_eq!(planning.meta.status, Status::Planning);
    assert!(planning.plan.is_none());

    // 2. The `agent-plan` effect drafts headlessly.
    let session_ref = ents_model::namespace::agent_session_ref(&id).expect("valid");
    let tip = root
        .refs
        .get(session_ref.as_ref())
        .expect("readable")
        .expect("exists");
    let worker_author = gix::actor::Signature {
        name: "worker".into(),
        email: "worker@ents.test".into(),
        time: gix::date::Time {
            seconds: 200,
            offset: 0,
        },
    };
    let signer = git_ents::sign::Signer::load(&fixture.key_path).expect("loads");
    let scratch = tempfile::tempdir().expect("tempdir");
    let plan_run = plan_worker::run_agent_plan(
        &root.refs,
        &root.objects,
        &root.events,
        &DraftExecutor,
        scratch.path(),
        &[],
        "true",
        tip,
        &worker_author,
        &|payload| signer.sign(payload),
        root.mode(),
    )
    .expect("drafts");
    assert!(matches!(
        plan_run,
        plan_worker::AgentPlanOutcome::Drafted { .. }
    ));

    let drafted = agent::show(&root, &id).expect("shows");
    assert_eq!(drafted.meta.status, Status::Ready);
    assert!(drafted.awaiting_confirmation());
    assert!(
        drafted
            .plan
            .as_deref()
            .is_some_and(|plan| plan.contains("reproduce the flake"))
    );

    // 3. Confirm from what stands in for "a second request": an
    // independent `agent::confirm` call, exactly what a second HTTP
    // request to the web UI would make.
    agent::confirm(&root, &id, None, Some(fixture.key_path.clone())).expect("confirms");
    let queued = agent::show(&root, &id).expect("shows");
    assert!(queued.queued());

    // 4. `agent-exec`'s own claim succeeds against the now-queued session
    // and runs it to `done`.
    let queued_ref = ents_model::namespace::agent_session_ref(&id).expect("valid");
    let queued_tip = root
        .refs
        .get(queued_ref.as_ref())
        .expect("readable")
        .expect("exists");
    let exec_run = agent_worker::run_agent_exec(
        &root.refs,
        &root.objects,
        &root.events,
        &ExecExecutor,
        scratch.path(),
        &[],
        "true",
        queued_tip,
        MemberId::new("worker"),
        "sprite-1".to_owned(),
        &worker_author,
        &|payload| signer.sign(payload),
        Mode::Advisory,
    )
    .expect("claims and runs");
    assert!(matches!(
        exec_run,
        agent_worker::AgentRunOutcome::Finished { .. }
    ));

    let done = agent::show(&root, &id).expect("shows");
    assert_eq!(done.meta.status, Status::Done);
}
