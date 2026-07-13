//! The boot-time reconciliation scan (`receive.reconstructible`): the
//! reference proof that the obligation queue needs no state `receive`
//! itself did not already have available in the repository.

use ents_model::Effect;
use ents_query::{Evaluator, Query};
use gix::refs::FullName;
use gix_hash::ObjectId;
use gix_object::{CommitRef, Find, Kind};
use gix_ref_store::RefStoreRead;

use crate::error::{Error, Result};
use crate::sink::EventSink;

const EFFECTS_PREFIX: &str = "refs/meta/effects/";

/// Every effect definition currently readable under `refs/meta/effects/*`,
/// as `(name, parsed trigger)`.
///
/// An effect whose tree cannot be read, or whose `trigger` fails to parse,
/// is skipped rather than failing the scan: `receive.validation`
/// (`ents-model`, `ents-query`) is what keeps a *newly written* effect
/// well-formed, but a scan reused across every future push must stay
/// resilient to a pre-existing malformed one rather than let it take down
/// every push after it (the same spirit as `receive.never-blocks`, applied
/// to a scan that runs on `receive`'s own hot path).
// @relation(receive.event-sink, scope=function)
fn known_effects(refs: &dyn RefStoreRead, objects: &impl Find) -> Result<Vec<(String, Query)>> {
    let mut effects = Vec::new();
    for entry in refs.iter_prefix(EFFECTS_PREFIX)? {
        let (name, tip) = entry?;
        let Some(short) = short_effect_name(&name) else {
            continue;
        };
        let Some(tree) = commit_tree(objects, tip)? else {
            continue;
        };
        let Ok(effect) = facet_git_tree::deserialize::<Effect>(&tree, objects) else {
            continue;
        };
        let Ok(trigger) = effect.trigger.parse::<Query>() else {
            continue;
        };
        effects.push((short, trigger));
    }
    Ok(effects)
}

/// The effect name segment of a `refs/meta/effects/<name>` refname, or
/// `None` for anything deeper or shallower (mirrors the results-namespace
/// scan in `ents-query`'s evaluator).
fn short_effect_name(name: &FullName) -> Option<String> {
    let path = name.as_bstr().to_string();
    let short = path.strip_prefix(EFFECTS_PREFIX)?;
    (!short.is_empty() && !short.contains('/')).then(|| short.to_owned())
}

/// The tree of the commit at `oid`, or `None` if `oid` is missing or not a
/// commit — treated as "this ref is unreadable", never a hard failure of
/// the whole scan. Shared with [`crate::receive`]'s redaction-target scan.
pub(crate) fn commit_tree(objects: &impl Find, oid: ObjectId) -> Result<Option<ObjectId>> {
    let mut buf = Vec::new();
    let Some(data) = objects
        .try_find(&oid, &mut buf)
        .map_err(|source| Error::Decode {
            oid,
            detail: source.to_string(),
        })?
    else {
        return Ok(None);
    };
    if data.kind != Kind::Commit {
        return Ok(None);
    }
    let Ok(commit) = CommitRef::from_bytes(data.data, oid.kind()) else {
        return Ok(None);
    };
    Ok(Some(commit.tree()))
}

/// The full, reconciliation-grade obligation scan
/// (`receive.reconstructible`): for every effect currently defined, compute
/// its outstanding work set (`trigger − results(self, any)`,
/// `query.workset`) against current ref state and enqueue every commit
/// still owed a result.
///
/// A composition root calls this once at startup, before serving further
/// pushes, so an `EventSink` that lost its queued events on crash (the null
/// sink always; the in-memory reference sink after a restart) recovers
/// exactly the same obligations incremental `receive` calls would have
/// enqueued — the queue is reconstructible from repository state alone,
/// with the dedup key (`receive.dedup`) unchanged by reconciliation.
///
/// # Errors
///
/// Fails only on a ref-store or object-store read failure, or a sink
/// failure; a malformed individual effect definition is skipped, not an
/// error (this module's private effect-scan helper treats an unreadable
/// tree or an unparsable trigger as "no match", never a hard failure).
///
/// # Examples
///
/// ```
/// use ents_model::Effect;
/// use ents_receive::{MemoryEventSink, reconcile};
/// use ents_testutil::{MemRefStore, ObjectStore, advance_ref, write_meta_entity};
///
/// let refs = MemRefStore::default();
/// let objects = ObjectStore::default();
/// let commits = advance_ref(&refs, &objects, "refs/heads/main", 1, 100);
///
/// let effect = Effect {
///     name: "unit".to_owned(),
///     trigger: "rev(refs/heads/main)".to_owned(),
///     toolchains: vec![],
///     run: "true".to_owned(),
/// };
/// let name: gix::refs::FullName = "refs/meta/effects/unit".try_into().expect("valid");
/// write_meta_entity(&refs, &objects, name, &effect, None, 200);
///
/// let sink = MemoryEventSink::default();
/// reconcile(&refs, &objects, &sink).expect("reconciles");
/// assert_eq!(sink.pending(), vec![("unit".to_owned(), commits[0])]);
/// ```
// @relation(receive.reconstructible, query.workset, scope=function)
pub fn reconcile(
    refs: &dyn RefStoreRead,
    objects: &impl Find,
    events: &dyn EventSink,
) -> Result<()> {
    let evaluator = Evaluator::new(refs, objects);
    for (name, trigger) in known_effects(refs, objects)? {
        for oid in evaluator.outstanding(&name, &trigger)? {
            enqueue(events, &name, oid)?;
        }
    }
    Ok(())
}

/// Enqueue matches for `transition` against every known effect
/// (`receive.event-sink`): the incremental counterpart to [`reconcile`],
/// called by [`crate::receive`] once per successfully applied transition.
// @relation(receive.event-sink, query.workset, scope=function)
pub(crate) fn enqueue_matches(
    refs: &dyn RefStoreRead,
    objects: &impl Find,
    events: &dyn EventSink,
    transition: &ents_query::Transition,
) -> Result<()> {
    let evaluator = Evaluator::new(refs, objects);
    for (name, trigger) in known_effects(refs, objects)? {
        for oid in evaluator.work_set(&name, &trigger, transition)? {
            enqueue(events, &name, oid)?;
        }
    }
    Ok(())
}

fn enqueue(events: &dyn EventSink, effect: &str, oid: ObjectId) -> Result<()> {
    events.enqueue(effect, oid)
}
