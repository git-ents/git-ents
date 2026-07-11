//! `receive`: the sole entry point through which a meta-ref or branch ref
//! is mutated (`receive.unit`).

use std::collections::HashSet;

use ents_gate::{Update, verify};
use ents_model::Redaction;
use ents_query::Transition;
use gix_hash::ObjectId;
use gix_object::Find;
use gix_ref_store::{Expected, RefEdit, RefStore, TxOutcome};

use crate::error::Result;
use crate::outcome::{Mode, Outcome, TxResult};
use crate::proposal::Proposal;
use crate::reconcile::{commit_tree, enqueue_matches};
use crate::sink::EventSink;

const REDACTIONS_PREFIX: &str = "refs/meta/redactions/";

/// The sole entry point through which a meta-ref or branch ref is mutated
/// (`receive.unit`).
///
/// Gate evaluation, redaction enforcement, effect-footprint matching, and
/// enqueue all live here, above the `RefStore` (`refs`), object-store
/// (`objects`), and [`EventSink`] (`events`) traits this function is
/// handed — never duplicated in a caller (`receive.unit`,
/// `arch.gate-receive-split`). Every mutation frontend (the CLI, the local
/// UI, a hosted smart-HTTP hook) MUST call exactly this function
/// in-process, with only the trait implementations and `mode` differing
/// (`receive.shared-path`): a `LooseRefStore` and a null sink locally, a
/// Postgres-backed store and a durable queue hosted.
///
/// # Order of operations
///
/// 1. **Redaction ingest** (`receive.redaction-ingest`): every object id
///    `proposal.objects` introduces is checked against the redaction
///    targets recorded under `refs/meta/redactions/*`. A match refuses the
///    *entire* batch before any verdict is even evaluated — a redacted
///    hole cannot be silently refilled by re-pushing the same bytes.
/// 2. **Gate evaluation** (`receive.refstore-seam`): every proposed
///    transition is judged by the identical [`ents_gate::verify`] every
///    other call site uses (`gate.call-sites`), read against `refs`'s read
///    half only.
/// 3. **Gate policy** (`Mode`): [`Mode::Mandatory`] aborts the whole batch
///    before writing anything if any verdict failed
///    (`gate.mandatory-hosted`); [`Mode::Advisory`] writes every transition
///    regardless of its verdict (`gate.advisory-local`) — the verdicts are
///    still returned for the caller to render.
/// 4. **Atomic write**: every transition lands as one
///    [`RefStore::transaction`] call, so either the whole batch applies or
///    none of it does (`gate.atomic-cas`). A stale precondition — another
///    writer moved a ref between step 2 and here — surfaces as
///    [`TxResult::Rejected`], in the gate's own vocabulary
///    (`Requirement::AtomicCas`).
/// 5. **Enqueue** (`receive.event-sink`, `receive.never-blocks`): once the
///    batch is durably applied, every known effect's static footprint is
///    matched against each transition and the entry set — `trigger −
///    results(self, any)`, `query.workset` — is enqueued into `events`.
///    This is the entire synchronous cost added to the write; no effect is
///    evaluated here.
///
/// # Object access
///
/// Per `receive.object-access`, object access here uses only gitoxide's
/// own traits — [`Find`] and [`gix_object::Write`] — never a private
/// object-access trait. `gix_object::Exists` is gitoxide's third named
/// trait for this seam; it is omitted from this signature because the
/// shared fixture (`ents_testutil::ObjectStore`, from the external
/// `facet-git-tree` crate) does not implement it — every existence check
/// this crate needs goes through `Find` instead (`try_find(..).is_some()`),
/// which is not a private trait and keeps `arch.no-object-store-trait`
/// intact.
///
/// Which object directory `objects` resolves through (the common odb, never
/// a git hook's quarantine directory, until its transaction commits) is the
/// composition root's responsibility to wire, not this function's: `receive`
/// only ever sees the seam it is handed.
///
/// # Errors
///
/// A [`crate::Error`] means `receive` could not reach an outcome at all
/// (a store or object read failed) — distinct from every variant of
/// [`Outcome`], which is a reached judgment.
///
/// # Examples
///
/// A minimal advisory, null-sink round trip: enroll an admin, set the
/// epoch, then land a signed issue mutation.
///
/// ```
/// use ents_gate::Config;
/// use ents_model::{Provenance, namespace};
/// use ents_receive::{Mode, NullEventSink, Proposal, RefTransition, TxResult, receive};
/// use ents_testutil::{Keypair, MemRefStore, ObjectStore, enroll_member, write_meta_entity};
///
/// // A stand-in for `ents-forge`'s `Issue` (this crate cannot depend on
/// // `ents-forge`, which itself depends on this crate): any
/// // Facet-derived entity exercises `receive`.
/// # #[derive(facet::Facet)]
/// # struct Issue { title: String, body: String, state: String }
/// #
/// let refs = MemRefStore::default();
/// let objects = ObjectStore::default();
/// let admin = Keypair::from_seed(1);
///
/// enroll_member(&refs, &objects, "admin", &admin, Provenance::AdminRegistered, 100);
/// let config_ref: gix::refs::FullName = namespace::CONFIG_REF.try_into().expect("valid");
/// write_meta_entity(&refs, &objects, config_ref, &Config { epoch: Some(200) }, Some(&admin), 200);
///
/// let issue = Issue {
///     title: "t".into(), body: "b".into(), state: "open".into(),
/// };
/// let name: gix::refs::FullName = "refs/meta/issues/1".try_into().expect("valid");
/// // write_meta_entity signs and lands the commit's object graph, but does
/// // not move the ref through the gate — that is exactly receive's job.
/// let tip = {
///     let tree = facet_git_tree::serialize_into(&issue, &objects).expect("serializes");
///     let trailers = ents_model::trailer::Trailers { ents_ref: Some(name.clone()), schema_version: None };
///     let message = format!("Mutate {}\n\n{}", name.as_bstr(), trailers.render());
///     ents_testutil::write_commit(&objects, &ents_testutil::CommitSpec {
///         tree, parents: vec![], message, seconds: 300,
///     }, Some(&admin))
/// };
///
/// let proposal = Proposal {
///     transitions: vec![RefTransition { name: name.clone(), old: None, new: Some(tip) }],
///     objects: vec![tip],
///     auth: None,
/// };
///
/// let outcome = receive(&refs, &objects, &NullEventSink, &proposal, Mode::Advisory).expect("evaluates");
/// assert_eq!(outcome.result, TxResult::Applied);
/// assert!(outcome.verdicts[0].1.is_pass());
/// ```
// @relation(receive.unit, receive.shared-path, receive.refstore-seam, receive.object-access, scope=function)
pub fn receive(
    refs: &dyn RefStore,
    objects: &(impl Find + gix_object::Write),
    events: &dyn EventSink,
    proposal: &Proposal,
    mode: Mode,
) -> Result<Outcome> {
    // @relation(receive.redaction-ingest, scope=function)
    if let Some(oid) = first_redacted(refs, objects, proposal)? {
        return Ok(Outcome {
            verdicts: Vec::new(),
            result: TxResult::Redacted { oid },
        });
    }

    let mut verdicts = Vec::with_capacity(proposal.transitions.len());
    let mut edits = Vec::with_capacity(proposal.transitions.len());
    let mut query_transitions = Vec::with_capacity(proposal.transitions.len());
    let mut any_failed = false;

    for transition in &proposal.transitions {
        let old = refs.get(transition.name.as_ref())?;
        // gate.call-sites: the identical function every other call site uses.
        // receive.redaction-admin-only is a consequence of this composition:
        // `verify` already refuses refs/meta/redactions/* to a non-admin
        // signer via its default namespace-authorization arm, regardless of
        // any refs/meta/config role rule, so no separate check is needed
        // here.
        // @relation(gate.call-sites, receive.redaction-admin-only, scope=function)
        let verdict = verify(
            refs,
            objects,
            &Update {
                name: transition.name.clone(),
                new: transition.new,
            },
        )?;
        any_failed |= !verdict.is_pass();
        verdicts.push((transition.name.clone(), verdict));

        let expected = old.map_or(Expected::MustNotExist, Expected::MustExistAndMatch);
        edits.push(RefEdit {
            name: transition.name.clone(),
            expected,
            new: transition.new,
        });
        query_transitions.push(Transition {
            name: transition.name.clone(),
            old,
            new: transition.new,
        });
    }

    // gate.mandatory-hosted: abort the whole batch before writing anything.
    // gate.advisory-local is the fallthrough: every transition is written
    // below regardless of `any_failed`.
    // @relation(gate.mandatory-hosted, scope=function)
    if any_failed && mode == Mode::Mandatory {
        return Ok(Outcome {
            verdicts,
            result: TxResult::Refused,
        });
    }

    let result = if edits.is_empty() {
        TxResult::Applied
    } else {
        match refs.transaction(&edits)? {
            TxOutcome::Applied => TxResult::Applied,
            TxOutcome::Rejected { name } => TxResult::Rejected { name },
        }
    };

    // receive.event-sink, receive.never-blocks: enqueue is the entire
    // synchronous cost; no effect is evaluated here.
    if result == TxResult::Applied {
        for transition in &query_transitions {
            enqueue_matches(refs, objects, events, transition)?;
        }
    }

    Ok(Outcome { verdicts, result })
}

/// The first object in `proposal.objects` that matches a target recorded
/// under `refs/meta/redactions/*`, if any (`receive.redaction-ingest`).
fn first_redacted(
    refs: &dyn gix_ref_store::RefStoreRead,
    objects: &impl Find,
    proposal: &Proposal,
) -> Result<Option<ObjectId>> {
    if proposal.objects.is_empty() {
        return Ok(None);
    }
    let mut targets = HashSet::new();
    for entry in refs.iter_prefix(REDACTIONS_PREFIX)? {
        let (_, tip) = entry?;
        let Some(tree) = commit_tree(objects, tip)? else {
            continue;
        };
        let Ok(redaction) = facet_git_tree::deserialize::<Redaction>(&tree, objects) else {
            continue;
        };
        targets.insert(redaction.target());
    }
    Ok(proposal
        .objects
        .iter()
        .copied()
        .find(|oid| targets.contains(oid)))
}
