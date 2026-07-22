//! The single-node hosted root's git-hook plumbing.
//!
//! The development plan's phase-6 row doubles `git-ents` as `git.ents.cloud`:
//! loose refs and a real odb on a Fly volume, served behind *git's own*
//! `receive-pack` — "the same stock-git transport Phase 0 bootstraps, now
//! invoking `receive()` from a hook" — with an in-memory `EventSink`, a
//! boot-time reconciliation scan, and the Sprite executor.
//!
//! # Why the ref write itself is not `ents_receive::receive`'s
//!
//! `receive.unit`'s own doc says every mutation frontend, "the CLI, the
//! local UI, a hosted smart-HTTP hook", must call `receive` in-process,
//! with only the trait implementations differing. That is true once the
//! store itself is swapped out from under git (`git-ents-server`, phase 8,
//! `gix-receive` replacing `receive-pack` entirely because a Postgres
//! `RefStore` leaves no on-disk repo for stock git to act on). Phase 6 is
//! explicitly *not* that case: this deployment keeps a real on-disk repo
//! and lets git's own `receive-pack` perform the actual object unpack and
//! ref update — "stock git wearing the same gate everything else runs, not
//! a bespoke protocol" (`docs/development-plan.adoc`).
//!
//! Concretely: if `pre_receive` here called `ents_receive::receive`
//! (writing the ref through *our own* `LooseRefStore::transaction`) and
//! then exited zero, git's `receive-pack` would still go on to perform its
//! own internal ref update afterward, expecting the ref to still hold the
//! *old* value it read before the hook ran — but we would have already
//! moved it. That double-write is a real race, not a hypothetical one, so
//! this module deliberately does not use `receive`'s bundled write path
//! for this deployment shape. Instead:
//!
//! - [`pre_receive`] calls the *identical* [`ents_gate::verify`] every
//!   other call site uses (`gate.call-sites`) for each proposed
//!   transition, and lets git's native `pre-receive` whole-push-rejection
//!   semantics implement `gate.mandatory-hosted` for free: refusing any
//!   one transition (nonzero exit, reasons on stderr) aborts the entire
//!   push before git writes anything, exactly what `Mode::Mandatory`
//!   means. On a pass, this hook writes nothing itself — git's own
//!   `receive-pack` performs the actual ref update once the hook exits
//!   zero.
//! - [`post_receive`] runs after git has already updated every ref: it
//!   opens a fresh [`crate::root::HostedRoot`] (whose `open` itself runs
//!   the boot-time [`ents_receive::reconcile`] scan,
//!   `receive.reconstructible`) and drains whatever is now outstanding,
//!   running each via the Sprite executor and writing results back
//!   through [`ents_effect::run::run_one`] — an ordinary `receive` client
//!   for the *results* ref, which never conflicts with a branch ref git
//!   itself just wrote.
//!
//! # Object visibility during `pre-receive` (quarantine)
//!
//! `receive.object-access`'s own doc flags "never a git hook's quarantine
//! directory, until its transaction commits" as the composition root's
//! responsibility. Git runs `pre-receive` with new objects visible only
//! through `GIT_OBJECT_DIRECTORY` (the quarantine) plus
//! `GIT_ALTERNATE_OBJECT_DIRECTORIES` (the real odb) until the push is
//! accepted; [`crate::root::HostedRoot::open`] honors `GIT_OBJECT_DIRECTORY`
//! when the environment sets it (which git does for `pre-receive`, and does
//! not for `post-receive`, whose objects are by then no longer quarantined).
//! `gix_odb::at` itself only ever follows a physical `info/alternates`
//! *file*, and git's own quarantine directory never has one — so this
//! crate's own [`crate::root::QuarantineObjects`] is what actually chains
//! the two directories, entirely in-process (no alternates file is ever
//! written to disk; see that type's own doc for why an earlier attempt at
//! writing one was wrong).
//!
//! # No separate daemon
//!
//! There is deliberately no long-lived worker process in this phase: each
//! hook invocation is a fresh, short-lived process that reconciles fresh
//! from repository state (`receive.reconstructible`'s own guarantee) —
//! "push-triggered" (the deployment table's own word for hosted execution)
//! without any inter-process queue at all. The literal "in-memory
//! `EventSink`" the development plan names lives for exactly one hook
//! invocation's lifetime; nothing about `receive.reconstructible`'s
//! contract requires it to live longer, and the phase-6 exit criterion —
//! obligations regenerate correctly after a `kill -9` of the in-memory
//! queue — is exactly what happens between every pair of pushes, verified
//! directly in this crate's tests by dropping a `HostedRoot` and opening a
//! fresh one against the same on-disk state.

#![expect(
    clippy::let_underscore_must_use,
    reason = "rejection reasons written to a hook's stderr are best-effort; a broken pipe here \
              is not actionable"
)]

use std::io::{BufRead, Read, Write};

use ents_effect::run::run_one;
use ents_model::Effect;
use ents_query::Query;
use ents_receive::Mode;
use gix::refs::FullName;
use gix_hash::ObjectId;
use gix_object::{CommitRef, Find, Kind};
use gix_ref_store::RefStoreRead;

use crate::error::{Error, Result};
use crate::root::HostedRoot;
use crate::sign::Signer;

/// One proposed transition, as read from git's `pre-receive` stdin: one
/// `<old-oid> <new-oid> <refname>` line per ref in the push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StdinTransition {
    /// The refname being updated.
    pub name: FullName,
    /// The proposed new tip, or `None` for a deletion.
    pub new: Option<ObjectId>,
}

/// Parse git's `pre-receive`/`post-receive` stdin format: one
/// `<old> <new> <refname>` line per updated ref.
///
/// # Errors
///
/// [`Error::InvalidArgument`] for a line that does not have exactly three
/// whitespace-separated fields, an unparsable oid, or an invalid refname.
pub fn parse_stdin_transitions(input: impl BufRead) -> Result<Vec<StdinTransition>> {
    let mut out = Vec::new();
    for line in input.lines() {
        let line = line.map_err(|source| Error::Io {
            path: "<stdin>".into(),
            source,
        })?;
        let mut fields = line.split_whitespace();
        let (Some(_old), Some(new), Some(name)) = (fields.next(), fields.next(), fields.next())
        else {
            return Err(Error::InvalidArgument(format!(
                "malformed pre-receive line: {line:?}"
            )));
        };
        let new: ObjectId = new
            .parse()
            .map_err(|_source| Error::InvalidArgument(format!("bad new oid: {new}")))?;
        let name: FullName = name
            .to_owned()
            .try_into()
            .map_err(|_source| Error::InvalidArgument(format!("bad refname: {name}")))?;
        let new = (!new.is_null()).then_some(new);
        out.push(StdinTransition { name, new });
    }
    Ok(out)
}

/// Run as git's own `pre-receive` hook (see this module's own doc for the
/// design). Reads transitions from `input`, evaluates the gate against
/// each, and refuses the whole push (returns `Err`) if any fails under the
/// mandatory gate. Rejection reasons are written to `report`.
///
/// # Errors
///
/// [`Error::Refused`] if any transition's verdict fails; propagates a
/// parse or gate-evaluation failure otherwise.
pub fn pre_receive(root: &HostedRoot, input: impl BufRead, mut report: impl Write) -> Result<()> {
    let transitions = parse_stdin_transitions(input)?;
    let mut failures = Vec::new();
    for transition in &transitions {
        let verdict = ents_gate::verify(
            &root.refs,
            &root.objects,
            &ents_gate::Update {
                name: transition.name.clone(),
                new: transition.new,
            },
        )?;
        if let ents_gate::Verdict::Fail(refusal) = verdict {
            let _ = writeln!(report, "refused: {refusal}");
            failures.push(refusal.to_string());
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(Error::Refused(failures.join("; ")))
    }
}

/// Run as git's own `post-receive` hook: reconcile outstanding effect
/// obligations and run every one of them via `executor`, writing each
/// result back through the ordinary `receive` path
/// (`effect.results-writeback`).
///
/// `root` must already have run its boot-time reconciliation scan (true of
/// any [`HostedRoot::open`]); this function additionally re-reconciles once
/// more before draining, so a push that itself just made new commits
/// outstanding is caught without waiting for the *next* process's boot.
///
/// # Errors
///
/// Propagates a reconciliation, toolchain-resolution, checkout, executor,
/// or write-back failure. A per-commit failure stops the drain at that
/// commit (mirrors [`ents_effect::run::run_effect`]'s own contract) —
/// results already written for earlier commits in this pass stay durable.
pub fn post_receive(
    root: &HostedRoot,
    executor: &dyn ents_effect::Executor,
    scratch: &std::path::Path,
    toolchain_cache: &std::path::Path,
    signer: &Signer,
) -> Result<usize> {
    ents_receive::reconcile(&root.refs, &root.objects, &root.events)?;

    let author = gix::actor::Signature {
        name: crate::root::HOSTED_WORKER_NAME.into(),
        email: "worker@git.ents.cloud".into(),
        time: gix::date::Time {
            seconds: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
                .unwrap_or_default(),
            offset: 0,
        },
    };

    let mut ran = 0usize;
    for (effect_name, oid) in root.events.pending() {
        let result_ref = ents_model::namespace::result_ref(&effect_name, &run_one_short(oid))?;
        // Skip work already resulted: the sink may re-list an obligation
        // whose result already landed in an earlier pass within the same
        // process (`receive.dedup`'s spirit — idempotent re-delivery,
        // never a duplicate effect run).
        if root.refs.get(result_ref.as_ref())?.is_some() {
            continue;
        }
        let Some(effect) = read_effect(&root.refs, &root.objects, &effect_name)? else {
            continue;
        };
        // `run_one`/`run_agent_exec` no longer resolve toolchain names
        // themselves: resolve and materialize this effect's declared
        // toolchains here, before handing the run loop an
        // already-materialized slice.
        let mut toolchains = Vec::with_capacity(effect.toolchains.len());
        for toolchain_name in &effect.toolchains {
            let (_, recipe) =
                ents_kiln::toolchain::resolve(&root.refs, &root.objects, toolchain_name)?;
            let bin = ents_kiln::toolchain::materialize(&recipe, &root.objects, toolchain_cache)?;
            toolchains.push((toolchain_name.clone(), bin));
        }

        // The `agent-exec` effect (`docs/agent-sessions-plan.adoc`'s Phase
        // 2) needs the bespoke dispatch/claim/finalize handling
        // `crate::agent_worker::run_agent_exec` provides — a plain
        // pass/fail result on a single ref, `run_one`'s own contract,
        // cannot express "claim, run a sandbox, and land the session's
        // terminal state, its result, and its result branch atomically."
        // Every other effect keeps the ordinary single-ref path.
        if effect_name == crate::agent_worker::AGENT_EXEC_NAME {
            crate::agent_worker::run_agent_exec(
                &root.refs,
                &root.objects,
                &root.events,
                executor,
                scratch,
                &toolchains,
                &effect.run,
                oid,
                ents_model::MemberId::new(crate::root::HOSTED_WORKER_NAME),
                crate::root::HOSTED_WORKER_NAME.to_owned(),
                &author,
                &|payload| signer.sign(payload),
                Mode::Mandatory,
            )?;
        } else if effect_name == crate::plan_worker::AGENT_PLAN_NAME {
            // The `agent-plan` effect (`docs/agent-sessions-plan.adoc`'s
            // Phase 4) needs the same bespoke handling `agent-exec` does,
            // for the same reason: a plain pass/fail result on a single
            // ref cannot express "draft a plan and land it atomically with
            // this effect's own result."
            crate::plan_worker::run_agent_plan(
                &root.refs,
                &root.objects,
                &root.events,
                executor,
                scratch,
                &toolchains,
                &effect.run,
                oid,
                &author,
                &|payload| signer.sign(payload),
                Mode::Mandatory,
            )?;
        } else {
            run_one(
                &root.refs,
                &root.objects,
                &root.events,
                executor,
                scratch,
                &toolchains,
                oid,
                &effect,
                result_ref,
                &author,
                |payload| signer.sign(payload),
                Mode::Mandatory,
            )?;
        }
        ran = ran.saturating_add(1);
    }
    Ok(ran)
}

/// The short-oid segment every results refname uses; mirrors
/// `ents_effect::run::short_oid` (private to that crate's own module path
/// from here, so this is a thin duplicate of a two-line slice operation
/// rather than a reason to change that crate's visibility).
fn run_one_short(oid: ObjectId) -> String {
    let hex = oid.to_string();
    hex.get(..12).unwrap_or(&hex).to_owned()
}

/// Read and parse the effect definition at `refs/meta/effects/<name>`, or
/// `None` if it is missing or malformed (mirrors `ents_receive::reconcile`'s
/// own tolerance for a pre-existing malformed effect).
fn read_effect(refs: &dyn RefStoreRead, objects: &impl Find, name: &str) -> Result<Option<Effect>> {
    let effect_ref = ents_model::namespace::effect_ref(name)?;
    let Some(tip) = refs.get(effect_ref.as_ref())? else {
        return Ok(None);
    };
    let mut buf = Vec::new();
    let Some(data) = objects
        .try_find(&tip, &mut buf)
        .map_err(|source| Error::InvalidArgument(source.to_string()))?
    else {
        return Ok(None);
    };
    if data.kind != Kind::Commit {
        return Ok(None);
    }
    let Ok(commit) = CommitRef::from_bytes(data.data, tip.kind()) else {
        return Ok(None);
    };
    let tree = commit.tree();
    let Ok(effect) = facet_git_tree::deserialize::<Effect>(&tree, objects) else {
        return Ok(None);
    };
    // Confirm the trigger still parses, mirroring `reconcile`'s own
    // tolerance rule; an effect whose trigger is unparsable is treated as
    // "nothing to run" — `None`, not a hard failure that would abort the
    // whole drain over one pre-existing malformed effect.
    if effect.trigger.parse::<Query>().is_err() {
        return Ok(None);
    }
    Ok(Some(effect))
}

/// A byte source the hook subcommands read stdin from — split out only so
/// tests can supply a fixed buffer instead of a real stdin handle.
pub fn read_all(mut input: impl Read) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    input.read_to_end(&mut buf).map_err(|source| Error::Io {
        path: "<stdin>".into(),
        source,
    })?;
    Ok(buf)
}
