//! Phase 6's redaction audit (`docs/agent-sessions-plan.adoc`): a per-member
//! BYOK credential (`roots.config-isolation`) is injected into a sandbox's
//! environment at launch (`SandboxInputs::env`) and must never be written
//! to repository data — every persisted artifact of a completed session
//! must be free of it.
//!
//! Two tests, deliberately asymmetric, spell out exactly what this system
//! guarantees and what it does not:
//!
//! - [`a_well_behaved_command_never_leaks_its_injected_credential`] proves
//!   the actual guarantee: the worker-side machinery
//!   (`git_ents::plan_worker::run_agent_plan`,
//!   `git_ents::agent_worker::run_agent_exec`) never itself writes the
//!   credential into any persisted artifact — every git object reachable
//!   from the session ref, both effects' own result refs, and the result
//!   branch, plus the on-disk scratch workdir left behind, are all swept
//!   for the sentinel and found clean.
//! - [`a_malicious_command_can_still_exfiltrate_its_own_env`] documents,
//!   honestly, the boundary that guarantee stops at: a command that
//!   deliberately echoes its own environment into its own log output or a
//!   file inside the workdir it controls gets that byte range faithfully
//!   recorded onto the result branch and the session's transcript — the
//!   system's job is injecting the credential at launch and never touching
//!   repository data with it itself, not sanitizing an adversarial
//!   command's own reported output. This test does not claim the system
//!   prevents that; it demonstrates the boundary so nobody has to take the
//!   first test's guarantee on faith beyond what it actually covers.

#![allow(clippy::expect_used, reason = "integration test")]

mod common;

use std::collections::HashSet;
use std::sync::Mutex;

use ents_effect::executor::SandboxInputs;
use ents_effect::run::short_oid;
use ents_effect::{Executor, RunOutput, RunStatus};
use ents_forge::agent::Status;
use ents_model::MemberId;
use ents_receive::Mode;
use git_ents::commands::agent;
use git_ents::credentials::{Credential, CredentialStore};
use git_ents::root::LocalRoot;
use git_ents::{agent_worker, plan_worker};
use gix_hash::ObjectId;
use gix_object::{Commit, CommitRef, Find, Kind, TreeRef};
use gix_ref_store::{Expected, RefEdit, RefStore, RefStoreRead as _};

/// The sentinel credential value: distinctive enough that any accidental
/// match is meaningful, shaped like a real Anthropic API key so this test
/// exercises the actual string shape a BYOK credential would have.
const SENTINEL: &str = "sk-ant-SENTINEL-0000000000000000-do-not-persist";
const VAR: &str = "ANTHROPIC_API_KEY";

/// Write an empty-tree commit and move `refname` to it directly through the
/// ref store — mirrors `tests/agent.rs`'s own identical helper (this
/// crate's own convention for a small per-file duplicate rather than a
/// shared `tests/common` addition for a single-use helper).
fn advance_branch(
    refs: &dyn RefStore,
    objects: &impl gix_object::Write,
    refname: &str,
    seconds: i64,
) -> ObjectId {
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

/// Recursively collect every blob's bytes and every commit's message,
/// reachable from `start` by following *every* parent (not just the first)
/// and every tree entry — a full reachability sweep of one git object
/// graph, not a single-tip spot check.
fn reachable_text(
    objects: &impl Find,
    start: ObjectId,
    seen: &mut HashSet<ObjectId>,
    out: &mut Vec<Vec<u8>>,
) {
    if !seen.insert(start) {
        return;
    }
    let mut buf = Vec::new();
    let Some(data) = Find::try_find(objects, &start, &mut buf).expect("object store reads") else {
        return;
    };
    match data.kind {
        Kind::Commit => {
            let commit = CommitRef::from_bytes(data.data, start.kind()).expect("commit parses");
            out.push(commit.message.to_vec());
            let tree = commit.tree();
            let parents: Vec<ObjectId> = commit.parents().collect();
            drop(commit);
            reachable_text(objects, tree, seen, out);
            for parent in parents {
                reachable_text(objects, parent, seen, out);
            }
        }
        Kind::Tree => {
            let tree = TreeRef::from_bytes(data.data, start.kind()).expect("tree parses");
            let children: Vec<ObjectId> = tree
                .entries
                .iter()
                .map(|entry| entry.oid.to_owned())
                .collect();
            drop(tree);
            for child in children {
                reachable_text(objects, child, seen, out);
            }
        }
        Kind::Blob => out.push(data.data.to_vec()),
        Kind::Tag => {}
    }
}

/// Whether `bytes` contains the sentinel credential, as a raw byte
/// substring (not a UTF-8 string comparison) so this catches the sentinel
/// regardless of what else surrounds it in a blob.
fn contains_sentinel(bytes: &[u8]) -> bool {
    let needle = SENTINEL.as_bytes();
    bytes.windows(needle.len()).any(|window| window == needle)
}

/// Assert that every object reachable from `tip` (including `tip` itself)
/// is free of the sentinel — `label` names what ref this tip came from, for
/// a legible failure.
fn assert_no_sentinel(objects: &impl Find, tip: ObjectId, label: &str) {
    let mut seen = HashSet::new();
    let mut blobs = Vec::new();
    reachable_text(objects, tip, &mut seen, &mut blobs);
    for blob in &blobs {
        assert!(
            !contains_sentinel(blob),
            "{label}: the sentinel credential leaked into a persisted git object reachable from {tip}"
        );
    }
}

/// A stub `Executor` for both `agent-plan` and `agent-exec`: records every
/// `env` pair it was launched with (so the test can confirm the credential
/// really was injected), and either behaves like an ordinary well-behaved
/// agent command (never echoes its own env anywhere) or, when `leak` is
/// set, deliberately writes its own env into a file in the workdir and
/// into its own log output — standing in for a buggy or malicious command,
/// never for the ordinary case.
struct RecordingExecutor {
    seen_env: Mutex<Vec<(String, String)>>,
    leak: bool,
    /// When set, write `plan_worker::AGENT_PLAN_DRAFT_FILE` so this stub
    /// also satisfies `run_agent_plan`'s own contract.
    drafts: bool,
}

impl Executor for RecordingExecutor {
    #[expect(
        clippy::unwrap_in_result,
        reason = "test fixture: a poisoned mutex or a workdir write failure here is a broken \
                  test, not a condition under test"
    )]
    fn run(&self, inputs: &SandboxInputs<'_>) -> ents_effect::Result<RunOutput> {
        self.seen_env
            .lock()
            .expect("uncontended in this test")
            .extend(inputs.env.iter().cloned());
        if self.drafts {
            std::fs::write(
                inputs.workdir.join(plan_worker::AGENT_PLAN_DRAFT_FILE),
                "1. reproduce\n2. fix\n3. verify",
            )
            .expect("write draft");
        }
        if self.leak {
            // Deliberately misbehaves: writes its own env into a file it
            // controls, and echoes it into its own reported output.
            let logged = inputs
                .env
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("\n");
            std::fs::write(inputs.workdir.join("leaked-by-the-command.txt"), &logged)
                .expect("write");
            return Ok(RunOutput {
                status: RunStatus::Pass,
                log: logged,
            });
        }
        Ok(RunOutput {
            status: RunStatus::Pass,
            log: "ordinary agent output, no secrets here".to_owned(),
        })
    }
}

/// Fixture: a fresh session, drafted and confirmed via a non-leaking
/// `RecordingExecutor`, with a `CredentialStore` configured for the
/// session's own member under [`SENTINEL`] — everything both tests share
/// before diverging on the `agent-exec` executor's own behavior.
struct Drafted {
    // Kept alive for this fixture's whole lifetime: `root` only borrows this
    // repository's path, it does not own the `TempDir` -- dropping `fixture`
    // early would delete the on-disk repository out from under `root`.
    _fixture: common::Fixture,
    root: LocalRoot,
    id: String,
    credentials: CredentialStore,
    worker_author: gix::actor::Signature,
    signer: git_ents::sign::Signer,
    scratch: tempfile::TempDir,
    /// The dequeued `planning` oid the headless draft ran against --
    /// `agent-plan`'s own results ref is keyed by this, not by `queued_tip`.
    planning_tip: ObjectId,
    queued_tip: ObjectId,
}

fn draft_and_confirm(seed: u8) -> Drafted {
    let fixture = common::Fixture::new(seed);
    let root = LocalRoot::open(fixture.path()).expect("opens");
    advance_branch(&root.refs, &root.objects, "refs/heads/main", 100);

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
    let member = agent::show(&root, &id).expect("shows").meta.member;

    let credentials = CredentialStore::from_pairs([(
        member,
        Credential {
            var: VAR.to_owned(),
            secret: SENTINEL.to_owned(),
        },
    )]);

    let session_ref = ents_model::namespace::agent_session_ref(&id).expect("valid");
    let planning_tip = root
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

    // Drafting is itself a credentialed run: a non-leaking executor here
    // keeps this fixture's own drafting step out of the redaction sweep's
    // way (its own credential handling is proven independently below).
    let draft_executor = RecordingExecutor {
        seen_env: Mutex::new(Vec::new()),
        leak: false,
        drafts: true,
    };
    let plan_run = plan_worker::run_agent_plan(
        &root.refs,
        &root.objects,
        &root.events,
        &draft_executor,
        scratch.path(),
        &[],
        "true",
        planning_tip,
        &worker_author,
        &|payload| signer.sign(payload),
        root.mode(),
        &credentials,
    )
    .expect("drafts");
    assert!(matches!(
        plan_run,
        plan_worker::AgentPlanOutcome::Drafted { .. }
    ));
    assert!(
        draft_executor
            .seen_env
            .lock()
            .expect("uncontended in this test")
            .iter()
            .any(|(k, v)| k == VAR && v == SENTINEL),
        "drafting must receive the injected credential too"
    );

    agent::confirm(&root, &id, None, Some(fixture.key_path.clone())).expect("confirms");
    let queued_ref = ents_model::namespace::agent_session_ref(&id).expect("valid");
    let queued_tip = root
        .refs
        .get(queued_ref.as_ref())
        .expect("readable")
        .expect("exists");

    Drafted {
        _fixture: fixture,
        root,
        id,
        credentials,
        worker_author,
        signer,
        scratch,
        planning_tip,
        queued_tip,
    }
}

/// The system's actual guarantee: worker-side machinery never itself writes
/// the injected BYOK credential into any persisted artifact. Sweeps the
/// session ref, both effects' own result refs, the result branch, and the
/// on-disk scratch remnants after the run.
// @relation(roots.config-isolation, effect.deployment-property, scope=function, role=Verifies)
#[test]
fn a_well_behaved_command_never_leaks_its_injected_credential() {
    let d = draft_and_confirm(60);

    let executor = RecordingExecutor {
        seen_env: Mutex::new(Vec::new()),
        leak: false,
        drafts: false,
    };
    let run = agent_worker::run_agent_exec(
        &d.root.refs,
        &d.root.objects,
        &d.root.events,
        &executor,
        d.scratch.path(),
        &[],
        "true",
        d.queued_tip,
        MemberId::new("worker"),
        "sprite-1".to_owned(),
        &d.worker_author,
        &|payload| d.signer.sign(payload),
        Mode::Advisory,
        &d.credentials,
    )
    .expect("claims and runs");
    assert!(matches!(
        run,
        agent_worker::AgentRunOutcome::Finished { .. }
    ));
    assert!(
        executor
            .seen_env
            .lock()
            .expect("uncontended in this test")
            .iter()
            .any(|(k, v)| k == VAR && v == SENTINEL),
        "the agent-exec run must actually receive the injected credential -- otherwise this \
         test would trivially pass by never exercising the seam at all"
    );

    let session = agent::show(&d.root, &d.id).expect("shows");
    assert_eq!(session.meta.status, Status::Done);

    // (a) the session ref: meta + plan + confirm + thread, all in the
    // tip's own tree (this design's typed-entity trees are rewritten whole
    // each commit, so the tip already carries the full thread history) --
    // plus every ancestor commit back to genesis, for a full sweep.
    let session_ref = ents_model::namespace::agent_session_ref(&d.id).expect("valid");
    let session_tip = d
        .root
        .refs
        .get(session_ref.as_ref())
        .expect("readable")
        .expect("exists");
    assert_no_sentinel(&d.root.objects, session_tip, "the session ref");

    // (b) the agent-exec effect's own result ref.
    let exec_results_ref =
        ents_model::namespace::result_ref(agent_worker::AGENT_EXEC_NAME, &short_oid(d.queued_tip))
            .expect("valid");
    let exec_results_tip = d
        .root
        .refs
        .get(exec_results_ref.as_ref())
        .expect("readable")
        .expect("exists");
    assert_no_sentinel(
        &d.root.objects,
        exec_results_tip,
        "the agent-exec result ref",
    );

    // (c) the agent-plan effect's own result ref, from the drafting step.
    let plan_results_ref =
        ents_model::namespace::result_ref(plan_worker::AGENT_PLAN_NAME, &short_oid(d.planning_tip))
            .expect("valid");
    let plan_results_tip = d
        .root
        .refs
        .get(plan_results_ref.as_ref())
        .expect("readable")
        .expect("the drafting step recorded a result");
    assert_no_sentinel(
        &d.root.objects,
        plan_results_tip,
        "the agent-plan result ref",
    );

    // (d) the result branch: the full tree contents the sandbox's own
    // output was captured into.
    let branch_name = session
        .meta
        .result_branch
        .clone()
        .expect("a result branch was recorded");
    let branch_ref: gix::refs::FullName = format!("refs/heads/{branch_name}")
        .try_into()
        .expect("valid");
    let branch_tip = d
        .root
        .refs
        .get(branch_ref.as_ref())
        .expect("readable")
        .expect("exists");
    assert_no_sentinel(&d.root.objects, branch_tip, "the result branch");

    // (e) the on-disk scratch workdir remnants: `run_agent_exec` cleans its
    // own workdir up after capturing the output tree, so nothing should be
    // left under `scratch/<oid>` at all -- belt and suspenders alongside
    // the git-object sweep above.
    assert!(
        !d.scratch.path().join(d.queued_tip.to_string()).exists(),
        "the run's own scratch workdir must not survive the run"
    );
}

/// Documents the honest boundary the guarantee above stops at: a command
/// that deliberately echoes its own environment into its own reported
/// output, or into a file inside the workdir it controls, gets that byte
/// range faithfully recorded onto the result branch and the session's own
/// transcript. This is not a bug the system fails to prevent -- the
/// transcript *is* the command's own stdout/stderr, and the result branch
/// *is* whatever the command left in its workdir; scrubbing either would
/// mean not recording what actually ran. The system's actual guarantee
/// (proven above) is narrower and does not cover this: it never injects the
/// credential into repository data itself, but it cannot stop an
/// adversarial command from exfiltrating its own environment through its
/// own reported output.
// @relation(roots.config-isolation, effect.deployment-property, scope=function, role=Verifies)
#[test]
fn a_malicious_command_can_still_exfiltrate_its_own_env() {
    let d = draft_and_confirm(61);

    let executor = RecordingExecutor {
        seen_env: Mutex::new(Vec::new()),
        leak: true,
        drafts: false,
    };
    let run = agent_worker::run_agent_exec(
        &d.root.refs,
        &d.root.objects,
        &d.root.events,
        &executor,
        d.scratch.path(),
        &[],
        "true",
        d.queued_tip,
        MemberId::new("worker"),
        "sprite-1".to_owned(),
        &d.worker_author,
        &|payload| d.signer.sign(payload),
        Mode::Advisory,
        &d.credentials,
    )
    .expect("claims and runs");
    assert!(matches!(
        run,
        agent_worker::AgentRunOutcome::Finished { .. }
    ));

    let session = agent::show(&d.root, &d.id).expect("shows");
    assert!(
        session.thread.iter().any(|blob| contains_sentinel(blob)),
        "the leaking command's own echoed log line lands in the transcript verbatim -- the \
         transcript is the command's own reported output, which this system faithfully records"
    );

    let branch_name = session
        .meta
        .result_branch
        .clone()
        .expect("a result branch was recorded");
    let branch_ref: gix::refs::FullName = format!("refs/heads/{branch_name}")
        .try_into()
        .expect("valid");
    let branch_tip = d
        .root
        .refs
        .get(branch_ref.as_ref())
        .expect("readable")
        .expect("exists");
    let mut seen = HashSet::new();
    let mut blobs = Vec::new();
    reachable_text(&d.root.objects, branch_tip, &mut seen, &mut blobs);
    assert!(
        blobs.iter().any(|blob| contains_sentinel(blob)),
        "the leaking command's own written file shows up on the result branch -- that file is \
         the command's own workdir output, which this system faithfully captures; it is not \
         something the worker-side machinery could have scrubbed without also discarding \
         genuine run output"
    );
}
