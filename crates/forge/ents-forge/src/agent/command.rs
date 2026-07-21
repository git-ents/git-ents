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

use super::{AgentSession, Confirm, ReviewPolicy, SessionMeta, Status, ToolchainPin};
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

/// `git ents agent confirm`: record a [`Confirm`] binding `id`'s current
/// plan hash, resolving the review policy to `review_policy` when given, or
/// to [`SessionMeta::review_policy`] otherwise.
///
/// # Errors
///
/// [`Error::NotFound`] if `id` has no session ref; [`Error::InvalidArgument`]
/// if the session is not `Ready`, or has no plan to confirm (a confirm can
/// never bind an absent plan leaf); otherwise propagates serialization or
/// `receive` failures.
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
