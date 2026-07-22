//! Integration coverage for `git ents agent` against a real local
//! composition root (`roots.local`): the plan-and-confirm ceremony
//! (`new`, `plan`, `confirm`) end to end, `list`/`show` reading it back,
//! and the guard that refuses confirming a session with no plan yet —
//! mirroring `tests/issue.rs`'s own shape for the same reason: every
//! operation here is `commands::agent`'s own library call
//! (`lens.parity`), not a re-implementation for the test.

#![allow(clippy::expect_used, reason = "integration test")]

mod common;

use ents_forge::agent::Status;
use git_ents::commands::agent;
use git_ents::root::LocalRoot;

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
