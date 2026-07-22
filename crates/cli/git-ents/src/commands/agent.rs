//! `git ents agent`: a thin wrapper around `ents_forge::agent`'s business
//! logic — this module only resolves the signer/actor identity (and, for
//! `new`, the session's own owning member, via the same key-to-member
//! lookup [`review::reviewer_member_id`](super::review) uses for a
//! review's reviewer) and translates a reached `Outcome` into a CLI-facing
//! [`Result`] (`crate::mutate::outcome_to_result`), exactly as every other
//! mutation command does. Every operation is the library call itself
//! (`lens.parity`); nothing here re-implements one.
//!
//! Only the plan-and-confirm ceremony (`new`, `plan`, `confirm`, `list`,
//! `show` — [`crate::cli::AgentAction`]'s own Phase 1 grammar) is wired
//! here. `claim` and `finish` are worker primitives
//! [`crate::agent_worker`] drives from the effect worker loop
//! (`crate::hook::post_receive`), not a porcelain a member runs by hand —
//! authorizing them to an interactive signer would only invite a
//! non-worker to forge a claim the gate's designated-worker roster exists
//! specifically to restrict (`docs/agent-sessions-plan.adoc`'s Phase 2a).
//! There is also no `git ents agent run` local-execution counterpart to
//! `git ents effect run`: the plan fixes Sprites as the sole execution
//! target for this feature and explicitly defers `effect.local-run`'s
//! parity requirement ("the tension ... is accepted for now and revisited
//! if local agent runs are ever wanted").

use std::path::PathBuf;

use ents_forge::agent::{self, AgentSession, NewAgentSession, ReviewPolicy};
use ents_model::MemberId;
use ents_receive::Identity;

use super::{actor, signer};
use crate::error::Result;
use crate::mutate::outcome_to_result;
use crate::root::LocalRoot;

/// `git ents agent list`: every agent session recorded in this repository.
///
/// # Errors
///
/// Propagates a ref-store or object read failure.
pub fn list(root: &LocalRoot) -> Result<Vec<(String, AgentSession)>> {
    Ok(agent::list(&root.refs, &root.objects)?)
}

/// `git ents agent show`: `id`'s agent session.
///
/// # Errors
///
/// [`crate::error::Error::Forge`] (wrapping [`ents_forge::Error::NotFound`])
/// if `id` has no session ref.
pub fn show(root: &LocalRoot, id: &str) -> Result<AgentSession> {
    Ok(agent::show(&root.refs, &root.objects, id)?)
}

/// `git ents agent new`: start an agent session owned by the signer's own
/// resolved member.
///
/// # Errors
///
/// [`crate::error::Error::Forge`] if a named toolchain has no
/// `refs/meta/toolchains/*` ref, or `retry_of` is given and is not a
/// well-formed oid; [`crate::error::Error::InvalidArgument`] if
/// `review_policy` is not `auto` or `manual`; otherwise see
/// [`crate::mutate::outcome_to_result`].
#[expect(
    clippy::too_many_arguments,
    reason = "one field per NewAgentSession-shaping CLI flag, mirroring git ents issue new's \
              identically-justified shape"
)]
pub fn new(
    root: &LocalRoot,
    prompt: String,
    model: String,
    toolchains: Vec<String>,
    base_ref: String,
    review_policy: String,
    retry_of: Option<String>,
    key: Option<PathBuf>,
) -> Result<String> {
    let signer = signer(root, key)?;
    let member = session_owner(root, &signer)?;
    let identity = Identity {
        actor: actor(&signer),
        author: None,
        sign: &|payload| signer.sign(payload),
    };
    let new = NewAgentSession {
        member,
        prompt,
        model,
        toolchains,
        base_ref,
        review_policy: review_policy.parse::<ReviewPolicy>()?,
        retry_of,
    };
    let (id, outcome) = agent::new(
        &root.refs,
        &root.objects,
        &root.events,
        new,
        &identity,
        root.mode(),
    )?;
    outcome_to_result(outcome, None)?;
    Ok(id)
}

/// `git ents agent plan`: draft or redraft `id`'s plan text.
///
/// # Errors
///
/// See [`crate::mutate::outcome_to_result`]; propagates
/// [`ents_forge::Error::NotFound`] or [`ents_forge::Error::InvalidArgument`]
/// (the session is past the point of no return) via [`crate::error::Error::Forge`].
pub fn plan(root: &LocalRoot, id: &str, text: String, key: Option<PathBuf>) -> Result<()> {
    let signer = signer(root, key)?;
    let identity = Identity {
        actor: actor(&signer),
        author: None,
        sign: &|payload| signer.sign(payload),
    };
    let outcome = agent::revise_plan(
        &root.refs,
        &root.objects,
        &root.events,
        id,
        text,
        &identity,
        root.mode(),
    )?;
    outcome_to_result(outcome, None)?;
    Ok(())
}

/// `git ents agent confirm`: confirm `id`'s current plan, queueing it for
/// execution.
///
/// # Errors
///
/// [`crate::error::Error::InvalidArgument`] if `review_policy` is given and
/// is not `auto` or `manual`; otherwise see
/// [`crate::mutate::outcome_to_result`].
pub fn confirm(
    root: &LocalRoot,
    id: &str,
    review_policy: Option<String>,
    key: Option<PathBuf>,
) -> Result<()> {
    let signer = signer(root, key)?;
    let identity = Identity {
        actor: actor(&signer),
        author: None,
        sign: &|payload| signer.sign(payload),
    };
    let policy = review_policy
        .map(|policy| policy.parse::<ReviewPolicy>())
        .transpose()?;
    let outcome = agent::confirm(
        &root.refs,
        &root.objects,
        &root.events,
        id,
        policy,
        &identity,
        root.mode(),
    )?;
    outcome_to_result(outcome, None)?;
    Ok(())
}

/// The member id owning the signer's key — the same key-to-member scan
/// `git ents review new`'s own `reviewer_member_id` performs, duplicated
/// rather than shared across the two command modules for the same reason
/// this codebase accepts other small per-module copies (see
/// `crate::commands::commit_tree`'s own doc): a member session's owner and
/// a review's reviewer are conceptually distinct fields that only happen
/// to resolve identically today.
///
/// # Errors
///
/// Propagates a ref-store or object read failure.
fn session_owner(root: &LocalRoot, signer: &crate::sign::Signer) -> Result<MemberId> {
    let pubkey = signer.public_openssh();
    if let Some((username, _state)) =
        super::members::find_by_key(&root.refs, &root.objects, &pubkey)?
    {
        return Ok(MemberId::new(username));
    }
    Ok(MemberId::new(super::short_fingerprint(signer)))
}
