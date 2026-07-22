//! The `agent-exec` effect's run path (`docs/agent-sessions-plan.adoc`'s
//! Phase 2): dispatch, claim, sandbox execution, and the atomic finalize
//! that lands the session's terminal state, its result record, and its
//! result branch in one [`ents_receive::receive`] call.
//!
//! # Why this lives here, not in `ents-effect` or `ents-forge`
//!
//! `ents-effect`'s own `Cargo.toml` links exactly `ents-model`,
//! `ents-query`, and `ents-receive` (`docs/spec/overview.adoc`'s crate-graph
//! table) — `ents-forge` is not among them, and must never become one
//! (`ents-forge`'s own `agent::dispatch` module doc states the same
//! constraint from the other side). Landing the session's `finish` and its
//! result record and its result branch in a *single* atomic proposal needs
//! both [`ents_forge::agent`]'s typed session commands and
//! [`ents_effect::Executor`]'s sandbox seam at once — a function that could
//! not live in either kernel crate without creating the wrong-direction
//! edge the crate graph forbids.
//!
//! `git-ents` already depends on both (it is the composition root every
//! other command module in this crate already wires stores and executors
//! for), so this module is the "session handler" seam the plan's Phase 2b
//! calls for: a callback the worker loop ([`crate::hook::post_receive`] for
//! the hosted root, [`crate::commands::agent::run`] for a local `git ents
//! agent run`) installs for the one effect name ([`AGENT_EXEC_NAME`]) whose
//! dispatched obligations need this bespoke handling, falling through to
//! the ordinary [`ents_effect::run::run_one`] path for every other effect.
//!
//! # How the confirmed plan reaches the sandbox
//!
//! [`ents_effect::executor::SandboxInputs::command`] is a fixed string (the
//! effect's own declared `run`); there is no existing convention for
//! threading per-invocation data through it. This module's own convention:
//! the confirmed plan text is written to [`AGENT_PLAN_FILE`] inside the
//! checked-out workdir *before* [`ents_effect::Executor::run`] is called —
//! the same host directory a backend's sandbox syncs in
//! ([`ents_effect::sprite::SpriteExecutor`]'s `sync_dir`), so the declared
//! command finds it already present once it starts, without this crate
//! templating untrusted plan text into a shell string. The file is removed
//! again before the sandbox's output is captured back into a tree (see
//! below), so it never leaks into the pushed result branch.
//!
//! # How the sandbox's output becomes the result branch
//!
//! [`ents_effect::Executor::run`] reports only pass/fail and a log — no
//! backend in this crate today reports back a modified filesystem. This
//! module reads the checked-out workdir's state *after* the command
//! completes and writes it back to a tree with
//! [`ents_effect::materialize::write_tree`] (`checkout`'s reverse), which is
//! correct for any backend whose command mutates `workdir` directly (true
//! of a real local run and of this module's own tests' stub executors).
//! [`ents_effect::sprite::SpriteExecutor`] does not yet sync a sandbox's
//! filesystem back onto its host `workdir` after a run — see that module's
//! own doc for the one-way `sync_dir` it has today — so a real Sprite
//! deployment of this effect needs that follow-up before the result
//! branch's tree reflects genuine sandbox output; this is called out again
//! in the module-level doc of `ents-effect`'s `sprite` module as a known
//! gap, not silently assumed away here.

use std::path::{Path, PathBuf};

use ents_effect::executor::SandboxInputs;
use ents_effect::run::short_oid;
use ents_effect::{Executor, RunStatus};
use ents_forge::agent::{
    AgentSession, ClaimAgentSession, Dispatch, FinishAgentSession, FinishOutcome, dispatch,
};
use ents_model::{MemberId, ResultRecord, Status as ResultStatus};
use ents_receive::{EventSink, Identity, Mode, Outcome, Proposal, RefTransition, TxResult};
use gix_hash::ObjectId;
use gix_object::{CommitRef, Find, Kind, Write, WriteTo as _};
use gix_ref_store::RefStore;

use crate::commands::commit_tree;
use crate::error::{Error, Result};

/// `agent-exec`'s own effect name — re-exported so a caller deciding
/// whether a dequeued `(effect, oid)` obligation belongs to this module's
/// bespoke handling, or the ordinary [`ents_effect::run::run_one`] path,
/// does not need a second import of `ents_effect::definition`.
pub const AGENT_EXEC_NAME: &str = ents_effect::definition::AGENT_EXEC_NAME;

/// The file the confirmed plan text is written to inside the checked-out
/// workdir before the sandboxed command runs (see this module's own doc).
/// Dot-prefixed to keep collisions with a real repository's own top-level
/// entries unlikely; removed again before the workdir's post-run state is
/// captured into the result branch's tree.
pub const AGENT_PLAN_FILE: &str = ".git-ents-agent-plan.txt";

/// What running the `agent-exec` effect against one dequeued `(effect,
/// oid)` obligation did.
#[derive(Debug)]
pub enum AgentRunOutcome {
    /// The tip was not queued-and-unclaimed; a cheap `pass` was recorded to
    /// discharge the obligation, no session was touched
    /// (`docs/agent-sessions-plan.adoc`'s Phase 2: "records a cheap `pass`
    /// no-op unless the tip is queued-and-unclaimed").
    NoOp,
    /// The tip was queued, but by the time this worker attempted to claim
    /// it, it no longer was (`Claim = CAS ...; first worker wins, losers
    /// no-op`) — a `pass` was recorded for the dequeued oid exactly as the
    /// no-op case above, and no session was touched.
    ClaimLost,
    /// This worker claimed the session, ran it, and finalized the run —
    /// `outcome` is the atomic multi-ref [`Outcome`] landing the session's
    /// terminal state, its result record, and its result branch together.
    Finished {
        /// The session's own genesis-oid id.
        id: String,
        /// The atomic finalize's outcome.
        outcome: Outcome,
    },
}

/// Run the `agent-exec` effect against the single dequeued commit `oid` — a
/// tip entering `refs/meta/agent-sessions/*` (`AGENT_EXEC_TRIGGER`'s own
/// `meta()` semantics: one obligation per ref tip, so `oid` is always a
/// session ref's own tip at match time).
///
/// Dispatch is pure and decided directly from `oid`'s own decoded session
/// tree (`ents_forge::agent::dispatch`), never from a second, possibly
/// racy re-read of the ref's current tip — the claim attempt below is what
/// re-reads fresh and is the sole place a race is judged.
///
/// `worker` and `sprite` become [`ents_forge::agent::SessionMeta::worker`]
/// and [`ents_forge::agent::SessionMeta::sprite`] on a successful claim.
/// `toolchains` and `command` are the `agent-exec` effect's own declared
/// toolchains (already resolved to host `bin/` directories, in declared
/// order — mirrors [`ents_effect::run::run_one`]'s own `toolchains`
/// parameter) and its declared `run` command; `scratch` is where the base
/// tree is checked out for one run, wiped and re-created per call exactly
/// like [`ents_effect::run::run_one`]'s own workdir.
///
/// # Errors
///
/// Any [`Error`] from reading or decoding the session, the claim attempt
/// (propagated unless it is the ordinary "no longer queued" precondition
/// miss, which this function turns into [`AgentRunOutcome::ClaimLost`]
/// instead), checking out the base tree, [`Executor::run`] itself — an
/// infrastructure failure here propagates with *nothing* published: the
/// session stays `Running`, and the queue's own retry policy is what
/// revisits it (`effect.result-taxonomy`) — or building and sending the
/// finalize proposal.
// @relation(effect.execution, effect.results-writeback, effect.result-taxonomy, receive.multi-ref-atomicity, scope=function)
#[expect(
    clippy::too_many_arguments,
    reason = "one input per materialization/identity step, mirrors ents_effect::run::run_one's \
              and run_effect's own shape"
)]
pub fn run_agent_exec<O>(
    refs: &dyn RefStore,
    objects: &O,
    events: &dyn EventSink,
    executor: &dyn Executor,
    scratch: &Path,
    toolchains: &[(String, PathBuf)],
    command: &str,
    oid: ObjectId,
    worker: MemberId,
    sprite: String,
    author: &gix::actor::Signature,
    sign: &dyn Fn(&[u8]) -> String,
    mode: Mode,
) -> Result<AgentRunOutcome>
where
    O: Find + Write,
{
    let tree = commit_tree(objects, oid)?;
    let session: AgentSession = facet_git_tree::deserialize(&tree, objects)?;
    let results_ref = ents_model::namespace::result_ref(AGENT_EXEC_NAME, &short_oid(oid))?;

    if dispatch(&session) == Dispatch::NoOp {
        record_pass(refs, objects, events, &results_ref, oid, author, sign, mode)?;
        return Ok(AgentRunOutcome::NoOp);
    }

    let id = genesis_of(objects, oid)?.to_string();
    let identity = Identity {
        actor: author.clone(),
        author: None,
        sign,
    };

    let claim_outcome = match ents_forge::agent::claim(
        refs,
        objects,
        events,
        &id,
        ClaimAgentSession { worker, sprite },
        &identity,
        mode,
    ) {
        Ok(outcome) => outcome,
        // The ordinary "first worker wins, losers no-op" race: by the time
        // this worker's claim re-read the session fresh, it was no longer
        // queued (`ents_forge::agent::claim`'s own precondition).
        Err(ents_forge::Error::InvalidArgument(_)) => {
            record_pass(refs, objects, events, &results_ref, oid, author, sign, mode)?;
            return Ok(AgentRunOutcome::ClaimLost);
        }
        Err(other) => return Err(other.into()),
    };
    if claim_outcome.result != TxResult::Applied {
        record_pass(refs, objects, events, &results_ref, oid, author, sign, mode)?;
        return Ok(AgentRunOutcome::ClaimLost);
    }

    // 3. Run the sandbox against the declared base ref's tip.
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

    let plan_path = workdir.join(AGENT_PLAN_FILE);
    std::fs::write(&plan_path, session.plan.as_deref().unwrap_or_default()).map_err(|source| {
        Error::Io {
            path: plan_path.clone(),
            source,
        }
    })?;

    let inputs = SandboxInputs {
        workdir: &workdir,
        toolchains,
        command,
    };
    // An `Err` here is an infrastructure failure the sandbox itself never
    // turned into a completed pass/fail (`effect.result-taxonomy`): it
    // propagates as-is, past every write below, so the finalize proposal
    // is never even built — the session stays `Running` (a legal
    // continuation, `docs/agent-sessions-plan.adoc`'s Phase 1b), the
    // queue's own retry bound decides what happens next.
    let output = executor.run(&inputs)?;

    // Never let the sideband plan file leak into the pushed result branch.
    // Best-effort: the command may already have deleted it itself, and
    // either way `write_tree` below must not see it.
    #[expect(
        clippy::let_underscore_must_use,
        reason = "best-effort cleanup; a failure here (already removed, or a transient host \
                  filesystem error) is not actionable and must not fail an otherwise-successful run"
    )]
    let _ = std::fs::remove_file(&plan_path);
    let output_tree = ents_effect::materialize::write_tree(objects, &workdir)?;

    let branch_name = result_branch_name(&session.meta.member, &id);
    let branch_ref: gix::refs::FullName =
        format!("refs/heads/{branch_name}")
            .try_into()
            .map_err(|_source| {
                Error::InvalidArgument(format!(
                    "computed result branch name {branch_name:?} is not a well-formed refname"
                ))
            })?;
    let output_commit = seal_commit(
        objects,
        output_tree,
        vec![base_tip],
        author,
        &format!("agent-exec output for session {id}"),
        sign,
    )?;

    let (status, finish_outcome) = match output.status {
        RunStatus::Pass => (ResultStatus::Pass, FinishOutcome::Done),
        RunStatus::Fail => (
            ResultStatus::Fail,
            FinishOutcome::Failed("the agent-exec command exited nonzero".to_owned()),
        ),
    };

    let finish = FinishAgentSession {
        outcome: finish_outcome,
        result_branch: Some(branch_name),
        thread: vec![output.log.into_bytes()],
    };
    let (finish_transition, finish_tip) =
        ents_forge::agent::finish_transition(refs, objects, &id, finish, &identity)?;

    let record = ResultRecord::new(AGENT_EXEC_NAME, oid, status);
    let (result_transition, result_tip) = ents_receive::entity_transition(
        refs,
        objects,
        &results_ref,
        &record,
        &identity,
        "Record agent-exec result",
    )?;

    let branch_old = refs.get(branch_ref.as_ref())?;
    let branch_transition = RefTransition {
        name: branch_ref,
        old: branch_old,
        new: Some(output_commit),
    };

    // Finalize = one atomic multi-ref proposal: the session's terminal
    // state, the result record, and the result branch land together or not
    // at all (`receive.multi-ref-atomicity`).
    let proposal = Proposal {
        transitions: vec![finish_transition, result_transition, branch_transition],
        objects: vec![finish_tip, result_tip, output_commit],
        auth: None,
    };
    let outcome = ents_receive::receive(refs, objects, events, &proposal, mode)?;
    Ok(AgentRunOutcome::Finished { id, outcome })
}

/// Record a cheap `pass` for `oid` on the canonical `agent-exec` results
/// ref — the no-op path both [`Dispatch::NoOp`] and a lost claim race take.
#[expect(
    clippy::too_many_arguments,
    reason = "one input per write_result parameter; a thin, single-call wrapper"
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
        AGENT_EXEC_NAME,
        oid,
        ResultStatus::Pass,
        author,
        sign,
        mode,
    )?)
}

/// The result branch name `docs/agent-sessions-plan.adoc`'s
/// resolved-by-default item fixes: `agent/<member>/<abbrev-genesis>`.
#[must_use]
fn result_branch_name(member: &MemberId, id: &str) -> String {
    format!("agent/{member}/{}", ents_forge::abbreviate_id(id))
}

/// Wipe and recreate `dir` — a fresh workdir per run, mirroring
/// [`ents_effect::run::run_one`]'s own "never inherit a previous run's
/// artifacts" rationale.
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
/// parent — the session's own genesis oid and id
/// (`meta-ref.identity-binding`), since every commit `ents_forge::agent`
/// writes onto a session ref is a straight-line, single-parent advance from
/// [`ents_forge::agent::new`]'s own parentless genesis.
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

/// Build, sign, and write a commit carrying `tree` and `parents` — the same
/// signing shape [`ents_receive`]'s own `signed_commit` and
/// `ents_effect::write_result`'s inline copy use, duplicated here rather
/// than shared cross-crate per this codebase's own convention (see
/// `ents_effect::results`'s module doc: "seals it ... exactly the way
/// `ents_sync::resolve::merge_heads` seals a merge tip"). This builds the
/// result branch's output commit specifically — not an entity, so
/// `ents_receive::entity_transition`'s typed-tree path does not apply to
/// it.
fn seal_commit(
    objects: &impl Write,
    tree: ObjectId,
    parents: Vec<ObjectId>,
    author: &gix::actor::Signature,
    subject: &str,
    sign: &dyn Fn(&[u8]) -> String,
) -> Result<ObjectId> {
    let mut commit = gix_object::Commit {
        tree,
        parents: parents.into(),
        author: author.clone(),
        committer: author.clone(),
        encoding: None,
        message: subject.to_owned().into(),
        extra_headers: Vec::new(),
    };
    let mut payload = Vec::new();
    commit.write_to(&mut payload).map_err(|source| {
        Error::InvalidArgument(format!(
            "serializing the result branch commit failed: {source}"
        ))
    })?;
    let pem = sign(&payload);
    commit
        .extra_headers
        .push(("gpgsig".into(), pem.trim_end().into()));

    let mut raw = Vec::new();
    commit.write_to(&mut raw).map_err(|source| {
        Error::InvalidArgument(format!(
            "serializing the signed result branch commit failed: {source}"
        ))
    })?;
    Ok(objects.write_buf(Kind::Commit, &raw)?)
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
    use ents_model::{Provenance, namespace};
    use ents_receive::NullEventSink;
    use ents_testutil::{
        Keypair, MemRefStore, ObjectStore, advance_ref, enroll_member, write_meta_entity,
    };
    use gix_ref_store::RefStoreRead as _;
    use rstest::rstest;

    use super::*;

    struct StubExecutor {
        status: RunStatus,
        mutate: fn(&Path),
    }

    impl Executor for StubExecutor {
        fn run(&self, inputs: &SandboxInputs<'_>) -> ents_effect::Result<ents_effect::RunOutput> {
            (self.mutate)(inputs.workdir);
            Ok(ents_effect::RunOutput {
                status: self.status,
                log: "agent transcript".to_owned(),
            })
        }
    }

    struct FailingExecutor;
    impl Executor for FailingExecutor {
        fn run(&self, _inputs: &SandboxInputs<'_>) -> ents_effect::Result<ents_effect::RunOutput> {
            Err(ents_effect::Error::Sandbox(
                "the sandbox never started".to_owned(),
            ))
        }
    }

    fn no_mutation(_workdir: &Path) {}

    fn writes_a_file(workdir: &Path) {
        std::fs::write(workdir.join("agent-output.txt"), b"the agent's work").expect("write");
    }

    /// Fixture: a member, an admin-registered worker, and `refs/heads/main`
    /// with one commit — everything a queued session needs to claim and
    /// run against.
    /// A detached signer over some bytes, returning an armored signature —
    /// mirrors `ents-forge`'s own `agent_sessions.rs` integration test
    /// fixture's identical type alias.
    type Signer = Box<dyn Fn(&[u8]) -> String>;

    struct Fixture {
        refs: MemRefStore,
        objects: ObjectStore,
        key: Keypair,
        sign: Signer,
        base: ObjectId,
    }

    impl Fixture {
        fn new() -> Self {
            let refs = MemRefStore::default();
            let objects = ObjectStore::default();
            let key = Keypair::from_seed(1);
            // `Keypair` deliberately carries no `Clone` (a real signing key
            // never should); `from_seed` is a pure function of the seed, so
            // a second, independent instance signs identically to `key`.
            let sign = Keypair::from_seed(1);
            enroll_member(
                &refs,
                &objects,
                "worker",
                &key,
                Provenance::AdminRegistered,
                100,
            );
            // The tip invariant only applies once an epoch is in force
            // (`gate.epoch`) — set one so the finalize's own gate checks
            // (`gate.fast-forward` in particular, the stale-CAS test below)
            // are genuinely exercised rather than short-circuited as
            // pre-epoch.
            let config_ref: gix::refs::FullName = namespace::CONFIG_REF.try_into().expect("valid");
            write_meta_entity(
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
            let commits = advance_ref(&refs, &objects, "refs/heads/main", 1, 200);
            let base = *commits.first().expect("advance_ref produced a commit");
            Self {
                refs,
                objects,
                key,
                sign: Box::new(move |payload: &[u8]| sign.sign(payload)),
                base,
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

        /// A brand-new, queued session (ready, plan confirmed) — the only
        /// precondition [`super::run_agent_exec`]'s `Dispatch::Claim` arm
        /// runs against.
        fn queued_session(&self) -> (ObjectId, String) {
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
    fn a_planning_session_is_a_cheap_no_op() {
        let fixture = Fixture::new();
        let identity = fixture.identity();
        let (id, outcome) = ents_forge::agent::new(
            &fixture.refs,
            &fixture.objects,
            &NullEventSink,
            ents_forge::agent::NewAgentSession {
                member: MemberId::new("jdc"),
                prompt: "prompt".to_owned(),
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
        let oid = fixture
            .refs
            .get(ref_name.as_ref())
            .expect("readable")
            .expect("exists");

        let author = fixture.author();
        let run = run_agent_exec(
            &fixture.refs,
            &fixture.objects,
            &NullEventSink,
            &FailingExecutor,
            Path::new("/does/not/matter"),
            &[],
            "true",
            oid,
            MemberId::new("worker"),
            "sprite-1".to_owned(),
            &author,
            fixture.sign_fn(),
            Mode::Advisory,
        )
        .expect("dispatch never touches the sandbox for a no-op");
        assert!(matches!(run, AgentRunOutcome::NoOp));

        let results_ref =
            ents_model::namespace::result_ref(AGENT_EXEC_NAME, &short_oid(oid)).expect("valid");
        assert!(
            fixture
                .refs
                .get(results_ref.as_ref())
                .expect("readable")
                .is_some(),
            "the no-op path must still discharge the obligation with a recorded pass"
        );
    }

    #[rstest]
    // @relation(scope=function, role=Verifies)
    fn a_lost_claim_race_records_a_pass_and_touches_no_session_state() {
        let fixture = Fixture::new();
        let (oid, id) = fixture.queued_session();

        // Simulate a second worker having already claimed the session
        // before this worker's own claim attempt runs.
        ents_forge::agent::claim(
            &fixture.refs,
            &fixture.objects,
            &NullEventSink,
            &id,
            ClaimAgentSession {
                worker: MemberId::new("someone-else"),
                sprite: "sprite-0".to_owned(),
            },
            &fixture.identity(),
            Mode::Advisory,
        )
        .expect("first claim succeeds");

        let author = fixture.author();
        let run = run_agent_exec(
            &fixture.refs,
            &fixture.objects,
            &NullEventSink,
            &FailingExecutor,
            Path::new("/does/not/matter"),
            &[],
            "true",
            oid,
            MemberId::new("worker"),
            "sprite-1".to_owned(),
            &author,
            fixture.sign_fn(),
            Mode::Advisory,
        )
        .expect("a lost race is not an error");
        assert!(matches!(run, AgentRunOutcome::ClaimLost));

        let session = ents_forge::agent::show(&fixture.refs, &fixture.objects, &id).expect("shows");
        assert_eq!(
            session.meta.worker,
            Some(MemberId::new("someone-else")),
            "the losing worker's identity must never land on the session"
        );
    }

    #[rstest]
    // @relation(effect.execution, effect.results-writeback, receive.multi-ref-atomicity, scope=function, role=Verifies)
    fn a_queued_session_lands_done_atomically_with_its_result_and_branch() {
        let fixture = Fixture::new();
        let (oid, id) = fixture.queued_session();

        let scratch = tempfile::tempdir().expect("tempdir");
        let executor = StubExecutor {
            status: RunStatus::Pass,
            mutate: writes_a_file,
        };
        let author = fixture.author();
        let run = run_agent_exec(
            &fixture.refs,
            &fixture.objects,
            &NullEventSink,
            &executor,
            scratch.path(),
            &[],
            "true",
            oid,
            MemberId::new("worker"),
            "sprite-1".to_owned(),
            &author,
            fixture.sign_fn(),
            Mode::Advisory,
        )
        .expect("runs and finalizes");
        let AgentRunOutcome::Finished {
            id: finished_id,
            outcome,
        } = run
        else {
            panic!("expected Finished, got {run:?}");
        };
        assert_eq!(finished_id, id);
        assert_eq!(outcome.result, TxResult::Applied);

        // (a) the session ref: Done, with the transcript appended.
        let session = ents_forge::agent::show(&fixture.refs, &fixture.objects, &id).expect("shows");
        assert_eq!(session.meta.status, ents_forge::agent::Status::Done);
        assert!(session.meta.finished.is_some());
        assert!(
            !session.thread.is_empty(),
            "the run's transcript must land as a thread blob"
        );
        let branch_name = session
            .meta
            .result_branch
            .clone()
            .expect("a result branch name was recorded");

        // (b) the result record, on the results namespace.
        let results_ref =
            ents_model::namespace::result_ref(AGENT_EXEC_NAME, &short_oid(oid)).expect("valid");
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

        // (c) the result branch, pointing at a commit whose tree carries
        // the sandbox's actual output.
        let branch_ref: gix::refs::FullName = format!("refs/heads/{branch_name}")
            .try_into()
            .expect("valid");
        let branch_tip = fixture
            .refs
            .get(branch_ref.as_ref())
            .expect("readable")
            .expect("branch landed");
        let branch_tree = commit_tree(&fixture.objects, branch_tip).expect("tree");
        let checkout_dir = tempfile::tempdir().expect("tempdir");
        ents_effect::materialize::checkout(&fixture.objects, branch_tree, checkout_dir.path())
            .expect("checkout");
        assert_eq!(
            std::fs::read_to_string(checkout_dir.path().join("agent-output.txt"))
                .expect("read the agent's own output"),
            "the agent's work"
        );
        assert!(
            !checkout_dir.path().join(AGENT_PLAN_FILE).exists(),
            "the sideband plan file must never leak into the pushed branch"
        );
    }

    #[rstest]
    // @relation(effect.result-taxonomy, scope=function, role=Verifies)
    fn a_failed_run_lands_the_session_as_failed_with_a_fail_result() {
        let fixture = Fixture::new();
        let (oid, id) = fixture.queued_session();

        let scratch = tempfile::tempdir().expect("tempdir");
        let executor = StubExecutor {
            status: RunStatus::Fail,
            mutate: no_mutation,
        };
        let author = fixture.author();
        run_agent_exec(
            &fixture.refs,
            &fixture.objects,
            &NullEventSink,
            &executor,
            scratch.path(),
            &[],
            "true",
            oid,
            MemberId::new("worker"),
            "sprite-1".to_owned(),
            &author,
            fixture.sign_fn(),
            Mode::Advisory,
        )
        .expect("runs and finalizes");

        let session = ents_forge::agent::show(&fixture.refs, &fixture.objects, &id).expect("shows");
        assert!(matches!(
            session.meta.status,
            ents_forge::agent::Status::Failed(_)
        ));

        let results_ref =
            ents_model::namespace::result_ref(AGENT_EXEC_NAME, &short_oid(oid)).expect("valid");
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
    fn a_sandbox_infrastructure_failure_publishes_nothing_and_leaves_the_session_running() {
        let fixture = Fixture::new();
        let (oid, id) = fixture.queued_session();

        let scratch = tempfile::tempdir().expect("tempdir");
        let author = fixture.author();
        let error = run_agent_exec(
            &fixture.refs,
            &fixture.objects,
            &NullEventSink,
            &FailingExecutor,
            scratch.path(),
            &[],
            "true",
            oid,
            MemberId::new("worker"),
            "sprite-1".to_owned(),
            &author,
            fixture.sign_fn(),
            Mode::Advisory,
        )
        .expect_err("an infra failure must propagate, not silently finalize");
        assert!(matches!(error, Error::Effect(_)));

        // The claim already landed (the session is Running, the point of
        // no return) but nothing past it did: a legal continuation per
        // Phase 1b's model, never a partially-published finalize.
        let session = ents_forge::agent::show(&fixture.refs, &fixture.objects, &id).expect("shows");
        assert_eq!(session.meta.status, ents_forge::agent::Status::Running);
        assert!(session.meta.result_branch.is_none());

        let results_ref =
            ents_model::namespace::result_ref(AGENT_EXEC_NAME, &short_oid(oid)).expect("valid");
        assert!(
            fixture
                .refs
                .get(results_ref.as_ref())
                .expect("readable")
                .is_none(),
            "no result may be recorded when the sandbox never completed the run"
        );
    }

    /// `receive.multi-ref-atomicity`'s own guarantee, demonstrated directly
    /// at the mechanism [`super::run_agent_exec`]'s finalize relies on: a
    /// [`ents_forge::agent::finish_transition`] built against a since-moved
    /// session tip fails the gate's `gate.fast-forward` check under a
    /// mandatory-mode `receive`, refusing the *entire* batch before any of
    /// the three transitions — session, result, branch — writes anything.
    #[rstest]
    // @relation(receive.multi-ref-atomicity, gate.fast-forward, scope=function, role=Verifies)
    fn a_stale_finish_transition_refuses_the_whole_finalize_batch() {
        let fixture = Fixture::new();
        let (oid, id) = fixture.queued_session();
        let identity = fixture.identity();

        ents_forge::agent::claim(
            &fixture.refs,
            &fixture.objects,
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

        // Build the finish transition against the tip as claimed above...
        let (finish_transition, finish_tip) = ents_forge::agent::finish_transition(
            &fixture.refs,
            &fixture.objects,
            &id,
            FinishAgentSession {
                outcome: FinishOutcome::Done,
                result_branch: Some("agent/jdc/deadbee".to_owned()),
                thread: vec![b"transcript".to_vec()],
            },
            &identity,
        )
        .expect("builds");

        // ...then race: another signed commit lands on the session ref
        // (an admin editing the session, say) before this finalize is
        // actually sent.
        let session_ref = ents_model::namespace::agent_session_ref(&id).expect("valid");
        let raced_tip = ents_testutil::write_commit(
            &fixture.objects,
            &ents_testutil::CommitSpec {
                tree: commit_tree(&fixture.objects, finish_tip).expect("tree"),
                parents: vec![
                    fixture
                        .refs
                        .get(session_ref.as_ref())
                        .expect("readable")
                        .expect("exists"),
                ],
                message: "a racing admin edit".to_owned(),
                seconds: 1_100,
            },
            Some(&fixture.key),
        );
        fixture.refs.set(session_ref.as_ref(), raced_tip);

        let results_ref =
            ents_model::namespace::result_ref(AGENT_EXEC_NAME, &short_oid(oid)).expect("valid");
        let record = ResultRecord::new(AGENT_EXEC_NAME, oid, ResultStatus::Pass);
        let (result_transition, result_tip) = ents_receive::entity_transition(
            &fixture.refs,
            &fixture.objects,
            &results_ref,
            &record,
            &identity,
            "Record agent-exec result",
        )
        .expect("builds");

        let branch_ref: gix::refs::FullName =
            "refs/heads/agent/jdc/deadbee".try_into().expect("valid");
        let output_tree = ents_testutil::empty_tree(&fixture.objects);
        let output_commit = ents_testutil::write_commit(
            &fixture.objects,
            &ents_testutil::CommitSpec {
                tree: output_tree,
                parents: vec![fixture.base],
                message: "agent-exec output".to_owned(),
                seconds: 1_100,
            },
            Some(&fixture.key),
        );

        let proposal = Proposal {
            transitions: vec![
                finish_transition,
                result_transition,
                RefTransition {
                    name: branch_ref.clone(),
                    old: None,
                    new: Some(output_commit),
                },
            ],
            objects: vec![finish_tip, result_tip, output_commit],
            auth: None,
        };
        // Mandatory mode is what gives a failed verdict teeth: a stale
        // transition refuses the whole batch before any write, rather than
        // merely being annotated (`gate.mandatory-hosted`).
        let outcome = ents_receive::receive(
            &fixture.refs,
            &fixture.objects,
            &NullEventSink,
            &proposal,
            Mode::Mandatory,
        )
        .expect("evaluates");
        assert_eq!(outcome.result, TxResult::Refused);

        // Nothing published: the session ref still holds the race's own
        // tip, never our finish commit; no result, no branch.
        assert_eq!(
            fixture.refs.get(session_ref.as_ref()).expect("readable"),
            Some(raced_tip)
        );
        assert!(
            fixture
                .refs
                .get(results_ref.as_ref())
                .expect("readable")
                .is_none()
        );
        assert!(
            fixture
                .refs
                .get(branch_ref.as_ref())
                .expect("readable")
                .is_none()
        );
    }
}
