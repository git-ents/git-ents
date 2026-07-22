//! The `agent` command's business logic: start a session (`new`), draft or
//! redraft its plan (`revise_plan`, which drops any confirm bound to the
//! plan text it replaces), record a confirmation (`confirm`), list, and read
//! one back. Phase 1 of `docs/agent-sessions-plan.adoc` stops here: claiming
//! a session, running it, and landing a result are Phase 2's `ents-effect`
//! job, and this module writes no ref but the session's own.
//!
//! Generalized over the same trait-object/generic seam
//! `crate::issue::command` and `crate::review::command` use (`&dyn
//! RefStore`/`RefStoreRead`, `impl Find`/`Find + Write`, `&dyn
//! ents_receive::EventSink`), so a composition root wires the concrete
//! types and calls these functions, never the other way around
//! (`lens.parity`).

use ents_model::MemberId;
use ents_receive::{Identity, Mode, Outcome, propose_entity, propose_genesis};
use gix_hash::ObjectId;
use gix_object::{CommitRef, Find, Kind, Write};
use gix_ref_store::{RefStore, RefStoreRead};

use super::{
    AgentSession, Confirm, FailureReason, ReviewPolicy, SessionMeta, Status, ToolchainPin,
};
use crate::error::{Error, Result};

/// The tree of the commit at `oid` — duplicated from `crate::issue::command`'s
/// own copy; see that copy's doc for why this codebase accepts one small
/// copy per module rather than a shared helper.
fn commit_tree(objects: &impl Find, oid: ObjectId) -> Result<ObjectId> {
    let mut buf = Vec::new();
    let data = objects
        .try_find(&oid, &mut buf)
        .map_err(|source| Error::InvalidArgument(source.to_string()))?
        .ok_or_else(|| Error::NotFound {
            what: oid.to_string(),
        })?;
    if data.kind != Kind::Commit {
        return Err(Error::NotFound {
            what: oid.to_string(),
        });
    }
    let commit = CommitRef::from_bytes(data.data, oid.kind())
        .map_err(|source| Error::InvalidArgument(source.to_string()))?;
    Ok(commit.tree())
}

/// Read the [`AgentSession`] at `id`'s ref tip, or [`Error::NotFound`] when
/// no such ref exists.
fn session_at(refs: &dyn RefStoreRead, objects: &impl Find, id: &str) -> Result<AgentSession> {
    let ref_name = ents_model::namespace::agent_session_ref(id)?;
    let Some(tip) = refs.get(ref_name.as_ref())? else {
        return Err(Error::NotFound {
            what: format!("agent session {id}"),
        });
    };
    let tree = commit_tree(objects, tip)?;
    Ok(facet_git_tree::deserialize(&tree, objects)?)
}

/// What `git ents agent new` writes: the member starting the session, the
/// initial task prompt (seeded as the thread's first opaque turn — Phase 1
/// carries no separate `prompt` field on the entity itself), and the
/// genesis-time choices that freeze into [`SessionMeta`].
#[derive(Debug, Clone)]
pub struct NewAgentSession {
    /// The member starting the session.
    pub member: MemberId,
    /// The initial task prompt, seeded verbatim as `thread`'s first turn.
    pub prompt: String,
    /// The model id the run executes against.
    pub model: String,
    /// The names of the toolchains this run depends on
    /// (`refs/meta/toolchains/<name>`); each is resolved to its ref's
    /// current tip and hash-pinned into the session
    /// ([`ToolchainPin`]) at creation.
    pub toolchains: Vec<String>,
    /// The ref the run executes against as its starting point.
    pub base_ref: String,
    /// The session's initially resolved review policy (Phase 5 lets a
    /// member override it up to confirm time; Phase 1 only carries the
    /// field).
    pub review_policy: ReviewPolicy,
    /// The genesis oid of a prior session this one retries, if any.
    pub retry_of: Option<String>,
}

/// `git ents agent new`: start an agent session at
/// `refs/meta/agent-sessions/<id>`, where `<id>` is the oid of the session's
/// own genesis commit — sign-then-name, never a locally minted id
/// (`meta-ref.identity-binding`), the same shape [`crate::issue::new`] uses.
///
/// `meta.created` is `identity`'s own commit timestamp, never a
/// separately-supplied value: two calls built from identical `new` fields
/// and an identical `identity` (same actor, same timestamp, same signature)
/// serialize to byte-identical genesis commits and therefore the same oid —
/// the same-second double-submit lands as one session, no nonce required.
///
/// # Errors
///
/// [`Error::NotFound`] if a named toolchain has no `refs/meta/toolchains/*`
/// ref; [`Error::InvalidArgument`] if `new.retry_of` is given and is not a
/// well-formed oid; otherwise propagates serialization or `receive`
/// failures.
// @relation(meta-ref.identity-binding, meta-ref.typed-tree, lens.parity, scope=function)
pub fn new(
    refs: &dyn RefStore,
    objects: &(impl Find + Write),
    events: &dyn ents_receive::EventSink,
    new: NewAgentSession,
    identity: &Identity<'_>,
    mode: Mode,
) -> Result<(String, Outcome)> {
    let mut toolchains = Vec::with_capacity(new.toolchains.len());
    for name in &new.toolchains {
        let ref_name = ents_model::namespace::toolchain_ref(name)?;
        let tip = refs
            .get(ref_name.as_ref())?
            .ok_or_else(|| Error::NotFound {
                what: format!("toolchain {name}"),
            })?;
        toolchains.push(ToolchainPin::new(name.clone(), tip));
    }
    let retry_of = new
        .retry_of
        .as_deref()
        .map(|hex| {
            ObjectId::from_hex(hex.as_bytes())
                .map_err(|_source| Error::InvalidArgument(format!("not a genesis oid: {hex}")))
        })
        .transpose()?;

    let meta = SessionMeta::new(
        new.member,
        identity.actor.time.seconds,
        new.model,
        toolchains,
        new.base_ref,
        new.review_policy,
        retry_of,
    );
    let session = AgentSession {
        meta,
        plan: None,
        confirm: None,
        thread: vec![new.prompt.into_bytes()],
    };

    let (ref_name, outcome) = propose_genesis(
        refs,
        objects,
        events,
        &session,
        |oid| ents_model::namespace::agent_session_ref(&oid.to_string()),
        identity,
        "Start agent session",
        mode,
    )?;
    Ok((crate::genesis_id(&ref_name), outcome))
}

/// `git ents agent plan`: draft or redraft `id`'s plan text, committing the
/// plan leaf and transitioning the session to `Ready`.
///
/// Any confirm the session carried is dropped unconditionally — a plan
/// revision that happens to land on byte-identical text is a degenerate
/// case not worth special-casing, so this never compares the new text's
/// hash against the old confirm's before dropping it. That is what keeps
/// [`AgentSession::queued`] sound: a confirm surviving in the tree could
/// never outlive the plan hash it bound.
///
/// # Errors
///
/// [`Error::NotFound`] if `id` has no session ref; [`Error::InvalidArgument`]
/// if the session is past the point of no return (`Running`, `Done`, or
/// `Failed` — the plan may no longer move once a worker has claimed it);
/// otherwise propagates serialization or `receive` failures.
// @relation(lens.parity, scope=function)
pub fn revise_plan(
    refs: &dyn RefStore,
    objects: &(impl Find + Write),
    events: &dyn ents_receive::EventSink,
    id: &str,
    plan: String,
    identity: &Identity<'_>,
    mode: Mode,
) -> Result<Outcome> {
    let mut session = session_at(refs, objects, id)?;
    if !matches!(session.meta.status, Status::Planning | Status::Ready) {
        return Err(Error::InvalidArgument(format!(
            "agent session {id} is past the point of no return; its plan can no longer be revised"
        )));
    }
    session.plan = Some(plan);
    session.confirm = None;
    session.meta.status = Status::Ready;

    let ref_name = ents_model::namespace::agent_session_ref(id)?;
    Ok(propose_entity(
        refs,
        objects,
        events,
        ref_name,
        &session,
        identity,
        &format!("Revise plan for agent session {id}"),
        mode,
    )?)
}

/// The web planning-chat page's explicit un-queue action
/// (`docs/agent-sessions-plan.adoc`'s resolved-by-default item 1, and
/// Phase 4's "Iteration" bullet: "from `ready`, reopening chat or
/// requesting a redraft returns to `planning`"): return a `Ready` session
/// to `Planning`, dropping any confirm it carries — the same drop
/// [`revise_plan`] performs as a side effect of a plan-text change, offered
/// here on its own for a member who wants to resume the planning
/// conversation before having redrafted anything.
///
/// # Errors
///
/// [`Error::NotFound`] if `id` has no session ref; [`Error::InvalidArgument`]
/// if the session is not `Ready` — still `Planning` has nothing to reopen,
/// and `Running`, `Done`, or `Failed` are past the point of no return;
/// otherwise propagates serialization or `receive` failures.
// @relation(lens.parity, scope=function)
pub fn reopen(
    refs: &dyn RefStore,
    objects: &(impl Find + Write),
    events: &dyn ents_receive::EventSink,
    id: &str,
    identity: &Identity<'_>,
    mode: Mode,
) -> Result<Outcome> {
    let mut session = session_at(refs, objects, id)?;
    if session.meta.status != Status::Ready {
        return Err(Error::InvalidArgument(format!(
            "agent session {id} is not ready; only a ready session may be reopened for planning"
        )));
    }
    session.meta.status = Status::Planning;
    session.confirm = None;

    let ref_name = ents_model::namespace::agent_session_ref(id)?;
    Ok(propose_entity(
        refs,
        objects,
        events,
        ref_name,
        &session,
        identity,
        &format!("Reopen agent session {id} for planning"),
        mode,
    )?)
}

/// The web planning-chat page's message endpoint: append `blobs` — one
/// opaque chat turn each, exactly like the prompt turn [`new`] seeds — to
/// `id`'s `thread`, touching neither `plan`, `confirm`, nor
/// `meta.status`.
///
/// A session past the point where planning conversation may still mutate
/// it refuses outright rather than silently un-queueing it
/// (`docs/agent-sessions-plan.adoc`'s Phase 4 acceptance: "after confirm,
/// no endpoint accepts messages or revisions without the explicit
/// un-queue" — [`reopen`] and [`revise_plan`] are that explicit un-queue;
/// this function is deliberately not one).
///
/// # Errors
///
/// [`Error::NotFound`] if `id` has no session ref; [`Error::InvalidArgument`]
/// if the session is [`AgentSession::queued`], `Running`, `Done`, or
/// `Failed` — only `Planning`, or `Ready` while still
/// [`AgentSession::awaiting_confirmation`], accepts a new turn; otherwise
/// propagates serialization or `receive` failures.
// @relation(lens.parity, scope=function)
pub fn append_thread(
    refs: &dyn RefStore,
    objects: &(impl Find + Write),
    events: &dyn ents_receive::EventSink,
    id: &str,
    blobs: Vec<Vec<u8>>,
    identity: &Identity<'_>,
    mode: Mode,
) -> Result<Outcome> {
    let mut session = session_at(refs, objects, id)?;
    let chattable = match session.meta.status {
        Status::Planning => true,
        Status::Ready => session.awaiting_confirmation(),
        Status::Running | Status::Done | Status::Failed(_) => false,
    };
    if !chattable {
        return Err(Error::InvalidArgument(format!(
            "agent session {id} is queued, running, or terminal; a chat message may not mutate \
             it without an explicit un-queue"
        )));
    }
    session.thread.extend(blobs);

    let ref_name = ents_model::namespace::agent_session_ref(id)?;
    Ok(propose_entity(
        refs,
        objects,
        events,
        ref_name,
        &session,
        identity,
        &format!("Append chat turn(s) to agent session {id}"),
        mode,
    )?)
}

/// `git ents agent confirm`: record a [`Confirm`] binding `id`'s current
/// plan hash, resolving the review policy to `review_policy` when given, or
/// to [`SessionMeta::review_policy`] otherwise.
///
/// # Errors
///
/// [`Error::NotFound`] if `id` has no session ref; [`Error::InvalidArgument`]
/// if the session is not `Ready`, or has no plan to confirm, or its plan is
/// empty or all-whitespace (`docs/agent-sessions-plan.adoc`'s Phase 4
/// acceptance, "no confirm can bind an empty or absent plan leaf" — a
/// confirm can never bind an absent, or effectively absent, plan leaf);
/// otherwise propagates serialization or `receive` failures.
// @relation(lens.parity, scope=function)
pub fn confirm(
    refs: &dyn RefStore,
    objects: &(impl Find + Write),
    events: &dyn ents_receive::EventSink,
    id: &str,
    review_policy: Option<ReviewPolicy>,
    identity: &Identity<'_>,
    mode: Mode,
) -> Result<Outcome> {
    let mut session = session_at(refs, objects, id)?;
    if session.meta.status != Status::Ready {
        return Err(Error::InvalidArgument(format!(
            "agent session {id} is not ready to confirm"
        )));
    }
    let plan_is_bindable = session
        .plan
        .as_deref()
        .is_some_and(|text| !text.trim().is_empty());
    if !plan_is_bindable {
        return Err(Error::InvalidArgument(format!(
            "agent session {id} has no plan to confirm; a confirm may not bind an empty or \
             absent plan leaf"
        )));
    }
    let Some(hash) = session.plan_hash() else {
        return Err(Error::InvalidArgument(format!(
            "agent session {id} has no plan to confirm"
        )));
    };
    let policy = review_policy.unwrap_or(session.meta.review_policy);
    session.confirm = Some(Confirm::new(hash, policy));

    let ref_name = ents_model::namespace::agent_session_ref(id)?;
    Ok(propose_entity(
        refs,
        objects,
        events,
        ref_name,
        &session,
        identity,
        &format!("Confirm agent session {id}"),
        mode,
    )?)
}

/// What `git ents agent claim` needs: which worker is claiming the session,
/// and the sandbox's name for this run.
#[derive(Debug, Clone)]
pub struct ClaimAgentSession {
    /// The worker claiming the session — becomes [`SessionMeta::worker`].
    pub worker: MemberId,
    /// The sandbox's name for this run — becomes [`SessionMeta::sprite`].
    pub sprite: String,
}

/// `git ents agent claim`: a worker claims `id`'s session, advancing it to
/// `Running` with the worker, the sandbox name, and the claim's own
/// timestamp as [`SessionMeta::started`] — the point of no return past
/// which no plan revision or un-queue is legal
/// (`docs/agent-sessions-plan.adoc`'s Phase 2, "Claim = CAS a `running`
/// status commit through `receive`; first worker wins, losers no-op": a
/// second `claim` call against the same session finds it no longer
/// [`AgentSession::queued`], since the first claim already advanced its
/// status past `Ready`, so it refuses exactly like any other
/// precondition miss — no separate "already claimed" error variant is
/// needed).
///
/// This is the command layer only: legality is judged purely from the
/// decoded tip's own state, never from who is signing — the gate's
/// designated-worker roster (`docs/agent-sessions-plan.adoc`'s Phase 2a)
/// is what actually authorizes a worker's signature onto the ref; this
/// function would happily build the same commit for any caller, exactly
/// as [`confirm`] does not itself check that `identity` is the session's
/// own member.
///
/// # Errors
///
/// [`Error::NotFound`] if `id` has no session ref; [`Error::InvalidArgument`]
/// if the session is not queued — not `Ready`, or `Ready` without a confirm
/// binding the current plan (`AgentSession::queued`) — including a session
/// a prior claim already advanced to `Running` or past; otherwise
/// propagates serialization or `receive` failures.
// @relation(lens.parity, scope=function)
pub fn claim(
    refs: &dyn RefStore,
    objects: &(impl Find + Write),
    events: &dyn ents_receive::EventSink,
    id: &str,
    claim: ClaimAgentSession,
    identity: &Identity<'_>,
    mode: Mode,
) -> Result<Outcome> {
    let mut session = session_at(refs, objects, id)?;
    if !session.queued() {
        return Err(Error::InvalidArgument(format!(
            "agent session {id} is not queued; only a queued session (ready, with a confirm \
             binding its current plan) may be claimed"
        )));
    }
    session.meta.status = Status::Running;
    session.meta.worker = Some(claim.worker);
    session.meta.sprite = Some(claim.sprite);
    session.meta.started = Some(identity.actor.time.seconds);

    let ref_name = ents_model::namespace::agent_session_ref(id)?;
    Ok(propose_entity(
        refs,
        objects,
        events,
        ref_name,
        &session,
        identity,
        &format!("Claim agent session {id}"),
        mode,
    )?)
}

/// How a run ended, for [`finish`].
#[derive(Debug, Clone)]
pub enum FinishOutcome {
    /// The run completed and its result landed.
    Done,
    /// The run could not complete, or was refused, for the carried reason.
    Failed(String),
}

/// What `git ents agent finish` needs: the run's outcome, the result
/// branch the worker pushed (if any), and the execution transcript to
/// append to `thread/`.
#[derive(Debug, Clone)]
pub struct FinishAgentSession {
    /// How the run ended.
    pub outcome: FinishOutcome,
    /// The branch the worker pushed the run's commits to
    /// (`agent/<member>/<abbrev-genesis>`, per the plan's resolved-by-default
    /// item) — becomes [`SessionMeta::result_branch`] when given; `None`
    /// leaves any existing value untouched (a `Failed` run that never
    /// reached the point of pushing a branch has nothing to record here).
    pub result_branch: Option<String>,
    /// Opaque, verbatim execution transcript blobs to append to
    /// [`AgentSession::thread`] — never typed or decoded by this crate,
    /// exactly like the prompt turn [`new`] seeds.
    pub thread: Vec<Vec<u8>>,
}

/// `git ents agent finish`: a worker finishes `id`'s session, advancing it
/// to a terminal state — `Done`, or `Failed` with a reason — and recording
/// the finish's own timestamp as [`SessionMeta::finished`], the result
/// branch when the run produced one, and the run's execution transcript
/// appended to `thread/` (`docs/agent-sessions-plan.adoc`'s Phase 2,
/// "Finalize = one atomic multi-ref push: thread blobs + `done`/`failed`
/// meta into the session tree" — the session-tree half of that multi-ref
/// write; landing the result ref and the result branch alongside it in the
/// same [`ents_receive::receive`] proposal is the composition root's job,
/// this crate never holding key material or touching those other refs).
///
/// Like [`claim`], this is the command layer only: legality is judged from
/// the decoded tip's own state, never from who is signing.
///
/// # Errors
///
/// [`Error::NotFound`] if `id` has no session ref; [`Error::InvalidArgument`]
/// if the session is not `Running` — a session that was never claimed, or
/// one already finished, may not be finished again; otherwise propagates
/// serialization or `receive` failures.
// @relation(lens.parity, scope=function)
pub fn finish(
    refs: &dyn RefStore,
    objects: &(impl Find + Write),
    events: &dyn ents_receive::EventSink,
    id: &str,
    finish: FinishAgentSession,
    identity: &Identity<'_>,
    mode: Mode,
) -> Result<Outcome> {
    let (transition, tip) = finish_transition(refs, objects, id, finish, identity)?;
    let proposal = ents_receive::Proposal {
        transitions: vec![transition],
        objects: vec![tip],
        auth: None,
    };
    Ok(ents_receive::receive(
        refs, objects, events, &proposal, mode,
    )?)
}

/// Build (but do not send) the [`finish`] transition: the same validation
/// and tree-building `finish` itself does, returning the
/// [`ents_receive::RefTransition`] and the new tip's oid instead of
/// proposing it alone.
///
/// This is the seam a composition root uses to land the session's finish
/// alongside the run's result record and its result branch in one atomic
/// [`ents_receive::Proposal`] (`receive.multi-ref-atomicity`,
/// `docs/agent-sessions-plan.adoc`'s Phase 2 finalize: "one atomic
/// multi-ref receive proposal") — [`finish`] itself is this function plus
/// wrapping the single transition in its own one-transition proposal, for
/// a caller that only ever needs the session ref to move alone.
///
/// # Errors
///
/// See [`finish`] — identical.
pub fn finish_transition(
    refs: &dyn RefStore,
    objects: &(impl Find + Write),
    id: &str,
    finish: FinishAgentSession,
    identity: &Identity<'_>,
) -> Result<(ents_receive::RefTransition, ObjectId)> {
    let mut session = session_at(refs, objects, id)?;
    if session.meta.status != Status::Running {
        return Err(Error::InvalidArgument(format!(
            "agent session {id} is not running; only a running session may be finished"
        )));
    }
    session.meta.status = match finish.outcome {
        FinishOutcome::Done => Status::Done,
        FinishOutcome::Failed(detail) => Status::Failed(FailureReason { detail }),
    };
    session.meta.finished = Some(identity.actor.time.seconds);
    if finish.result_branch.is_some() {
        session.meta.result_branch = finish.result_branch;
    }
    session.thread.extend(finish.thread);

    let ref_name = ents_model::namespace::agent_session_ref(id)?;
    Ok(ents_receive::entity_transition(
        refs,
        objects,
        &ref_name,
        &session,
        identity,
        &format!("Finish agent session {id}"),
    )?)
}

/// The `agent-plan` effect's own commit (`docs/agent-sessions-plan.adoc`'s
/// Phase 4, "headless plan drafting ... commits the plan leaf and
/// transitions to `ready`"): like [`revise_plan`], but atomically appending
/// the drafting run's own transcript to `thread` in the same commit, and
/// requiring the session still be exactly `Planning` — the runner's own
/// dispatch precondition ([`super::dispatch_plan`]) — rather than
/// [`revise_plan`]'s looser `Planning`-or-`Ready`: a session a human has
/// already moved to `Ready` by hand raced ahead of this draft, and this
/// function refuses rather than clobbering it.
///
/// # Errors
///
/// [`Error::NotFound`] if `id` has no session ref; [`Error::InvalidArgument`]
/// if the session is not `Planning`; otherwise propagates serialization or
/// `receive` failures.
// @relation(lens.parity, scope=function)
#[expect(
    clippy::too_many_arguments,
    reason = "one field per draft input plus the ordinary refs/objects/events/identity/mode \
              quintet every mutation command in this module takes"
)]
pub fn draft_plan(
    refs: &dyn RefStore,
    objects: &(impl Find + Write),
    events: &dyn ents_receive::EventSink,
    id: &str,
    plan: String,
    transcript: Vec<Vec<u8>>,
    identity: &Identity<'_>,
    mode: Mode,
) -> Result<Outcome> {
    let (transition, tip) = draft_plan_transition(refs, objects, id, plan, transcript, identity)?;
    let proposal = ents_receive::Proposal {
        transitions: vec![transition],
        objects: vec![tip],
        auth: None,
    };
    Ok(ents_receive::receive(
        refs, objects, events, &proposal, mode,
    )?)
}

/// Build (but do not send) the [`draft_plan`] transition — the seam
/// `git-ents`'s `agent-plan` effect handler uses to land the session's
/// draft alongside its own results record in one atomic
/// [`ents_receive::Proposal`], exactly as [`finish_transition`] does for
/// `agent-exec`'s finalize.
///
/// # Errors
///
/// See [`draft_plan`] — identical.
pub fn draft_plan_transition(
    refs: &dyn RefStore,
    objects: &(impl Find + Write),
    id: &str,
    plan: String,
    transcript: Vec<Vec<u8>>,
    identity: &Identity<'_>,
) -> Result<(ents_receive::RefTransition, ObjectId)> {
    let mut session = session_at(refs, objects, id)?;
    if session.meta.status != Status::Planning {
        return Err(Error::InvalidArgument(format!(
            "agent session {id} is not planning; only a planning session may be auto-drafted"
        )));
    }
    session.plan = Some(plan);
    session.confirm = None;
    session.meta.status = Status::Ready;
    session.thread.extend(transcript);

    let ref_name = ents_model::namespace::agent_session_ref(id)?;
    Ok(ents_receive::entity_transition(
        refs,
        objects,
        &ref_name,
        &session,
        identity,
        &format!("Draft plan for agent session {id}"),
    )?)
}

/// `git ents agent list`: every agent session recorded in this repository.
///
/// A ref whose tip this build cannot read back as an [`AgentSession`] is
/// silently absent here; [`list_all`] is the caller-facing counterpart that
/// surfaces those refs instead of dropping them.
///
/// # Errors
///
/// Propagates a ref-store or object read failure.
pub fn list(refs: &dyn RefStoreRead, objects: &impl Find) -> Result<Vec<(String, AgentSession)>> {
    Ok(list_all(refs, objects)?.0)
}

/// [`list`] plus the refs it could not read: every readable agent session,
/// and one [`crate::Unreadable`] per `refs/meta/agent-sessions/*` ref whose
/// tip this build's [`AgentSession`] shape could not read back — mirroring
/// [`crate::issue::list_all`]'s never-silently-dropped contract (see
/// [`crate::Unreadable`]'s own doc).
///
/// # Errors
///
/// Propagates a ref-store read failure — a per-ref *entity* read failure is
/// a row in the second vec, never an error.
pub fn list_all(
    refs: &dyn RefStoreRead,
    objects: &impl Find,
) -> Result<crate::Listing<AgentSession>> {
    let mut out = Vec::new();
    let mut unreadable = Vec::new();
    for entry in refs.iter_prefix("refs/meta/agent-sessions/")? {
        let (name, tip) = entry?;
        let path = name.as_bstr().to_string();
        let Some(id) = path.strip_prefix("refs/meta/agent-sessions/") else {
            continue;
        };
        match commit_tree(objects, tip)
            .and_then(|tree| Ok(facet_git_tree::deserialize::<AgentSession>(&tree, objects)?))
        {
            Ok(session) => out.push((id.to_owned(), session)),
            Err(error) => unreadable.push(crate::Unreadable {
                refname: path.clone(),
                error: error.to_string(),
            }),
        }
    }
    Ok((out, unreadable))
}

/// `git ents agent show`: `id`'s agent session.
///
/// # Errors
///
/// [`Error::NotFound`] if `id` has no session ref.
pub fn show(refs: &dyn RefStoreRead, objects: &impl Find, id: &str) -> Result<AgentSession> {
    session_at(refs, objects, id)
}
