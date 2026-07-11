//! Fetch and push over `refs/meta/*` — the routine plumbing that moves the
//! forge itself, not merely code (`sync.forge-transfer`).
//!
//! Both directions copy the *complete* object closure of every meta-ref —
//! each ref's whole commit chain and all its trees and blobs, commit
//! objects verbatim so their signatures travel too — so a clone plus
//! `refs/meta/*` carries the entire audit history and the signatures needed
//! to verify it, with nothing left server-side (`sync.forge-transfer`).
//!
//! A remote and a local repository are each just a ([`RefStoreRead`]/
//! [`RefStore`], `Find`/`Write`) pair; transfer is expressed directly over
//! those seams, with no bespoke transport type. [`fetch`] advances every
//! local meta-ref that the remote fast-forwards and reports the ones that
//! diverged, feeding them to the merge machinery ([`crate::resolve`]).
//! [`push`] runs pre-flight against the remote's own policy before moving a
//! ref, so a rejected canonical push surfaces the inbox alternative instead
//! (`sync.pre-flight`, `sync.inbox-routing`).

use ents_gate::Update;
use ents_model::MemberId;
use gix::refs::FullName;
use gix_hash::ObjectId;
use gix_object::{Find, Write};
use gix_ref_store::{Expected, RefEdit, RefStore, RefStoreRead, TxOutcome};

use crate::error::Result;
use crate::objects::{copy_closure, descends_from};
use crate::preflight::{PreFlight, preflight};

/// The meta-ref prefix that scopes every forge transfer.
const META_PREFIX: &str = "refs/meta/";

/// A remote meta-ref that has moved out from under the local tip: neither
/// side descends from the other, so a fast-forward is impossible and the
/// answer is a merge ([`crate::resolve::merge_heads`], `sync.divergence-merge`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diverged {
    /// The ref that diverged.
    pub name: FullName,
    /// The local tip.
    pub local: ObjectId,
    /// The remote tip.
    pub remote: ObjectId,
}

/// What [`fetch`] did to the local `refs/meta/*` set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FetchReport {
    /// Refs advanced to the remote tip (created, or fast-forwarded).
    pub updated: Vec<FullName>,
    /// Refs already at the remote tip; nothing to do.
    pub unchanged: Vec<FullName>,
    /// Refs whose local and remote tips diverged — resolve by merging.
    pub diverged: Vec<Diverged>,
}

/// Fetch every `refs/meta/*` ref from `remote` into `local`, moving the
/// whole forge (`sync.forge-transfer`).
///
/// For each remote meta-ref the full object closure is copied into
/// `local_objects` first — so the ref never points at an object the local
/// store lacks — then the local ref is advanced if the remote is a
/// fast-forward, left alone if already current, and reported as
/// [`Diverged`] otherwise. Object copy is unconditional even for a
/// divergence, since the subsequent merge needs both heads present locally.
///
/// # Errors
///
/// Propagates ref-store and object failures; a stale CAS during the ref
/// update surfaces as an [`crate::Error`]-free skip is *not* done here —
/// fetch runs single-writer against the local store.
///
/// # Examples
///
/// ```
/// use ents_model::Provenance;
/// use ents_sync::transfer::fetch;
/// use ents_testutil::{Keypair, MemRefStore, ObjectStore, enroll_member};
/// use gix_ref_store::RefStoreRead;
///
/// // The "remote" is just another ref-store / object-store pair.
/// let remote_refs = MemRefStore::default();
/// let remote_objects = ObjectStore::default();
/// let key = Keypair::from_seed(1);
/// enroll_member(&remote_refs, &remote_objects, "jdc", &key, Provenance::AdminRegistered, 100);
///
/// let local_refs = MemRefStore::default();
/// let local_objects = ObjectStore::default();
/// let report = fetch(&remote_refs, &remote_objects, &local_refs, &local_objects).expect("fetches");
/// assert_eq!(report.updated.len(), 1);
///
/// let name: gix::refs::FullName = "refs/meta/member/jdc".try_into().expect("valid");
/// assert!(local_refs.get(name.as_ref()).expect("readable").is_some());
/// ```
// @relation(sync.forge-transfer, scope=function)
pub fn fetch(
    remote_refs: &dyn RefStoreRead,
    remote_objects: &impl Find,
    local_refs: &dyn RefStore,
    local_objects: &(impl Find + Write),
) -> Result<FetchReport> {
    let mut report = FetchReport::default();
    for entry in remote_refs.iter_prefix(META_PREFIX)? {
        let (name, remote_tip) = entry?;
        copy_closure(remote_objects, local_objects, remote_tip)?;

        let local_tip = local_refs.get(name.as_ref())?;
        match local_tip {
            Some(local) if local == remote_tip => report.unchanged.push(name),
            Some(local) if descends_from(local_objects, remote_tip, local)? => {
                advance(
                    local_refs,
                    &name,
                    Expected::MustExistAndMatch(local),
                    remote_tip,
                )?;
                report.updated.push(name);
            }
            Some(local) => report.diverged.push(Diverged {
                name,
                local,
                remote: remote_tip,
            }),
            None => {
                advance(local_refs, &name, Expected::MustNotExist, remote_tip)?;
                report.updated.push(name);
            }
        }
    }
    Ok(report)
}

/// The outcome of pushing one local meta-ref to a remote ([`push`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pushed {
    /// Pre-flight passed and the ref was transferred and advanced on the
    /// remote.
    Advanced(FullName),
    /// Pre-flight predicted a rejection the inbox can absorb; the ref was
    /// *not* pushed, and this is the inbox route offered instead
    /// (`sync.inbox-routing`).
    Inbox(FullName),
    /// Pre-flight predicted a rejection the inbox cannot absorb (a
    /// divergence — merge first — or a refname mismatch). The ref was not
    /// pushed; the prediction is carried for the caller to render.
    Refused(Box<PreFlight>),
}

/// Push one local meta-ref `name` to `remote`, pre-flighting against the
/// remote's own policy first (`sync.pre-flight`).
///
/// The local tip's object closure is copied to the remote only once
/// pre-flight passes; a predicted rejection routes to the inbox
/// (`sync.inbox-routing`) or is reported, and nothing is transferred. This
/// runs the identical gate the remote will run at CAS time
/// (`gate.call-sites`), so the result is a prediction that can only be
/// stale, never wrong about the rules. Local writes are never blocked by
/// this — that is [`mod@crate::preflight`]'s and the local store's concern
/// (`sync.local-advisory`); push is the one place a verdict gates an
/// actual (remote) write.
///
/// # Errors
///
/// Propagates pre-flight, ref-store, and object failures.
// @relation(sync.pre-flight, sync.inbox-routing, sync.forge-transfer, scope=function)
pub fn push(
    remote_refs: &dyn RefStore,
    remote_objects: &(impl Find + Write),
    local_objects: &impl Find,
    name: &FullName,
    local_tip: ObjectId,
    author: &MemberId,
) -> Result<Pushed> {
    let update = Update {
        name: name.clone(),
        new: Some(local_tip),
    };
    // Pre-flight needs the proposed objects visible in the store it reads,
    // exactly as the hosted CAS would after ingest; copy first, then judge.
    copy_closure(local_objects, remote_objects, local_tip)?;
    let pf = preflight(remote_refs, remote_objects, &update, author)?;
    if !pf.is_pass() {
        return Ok(match pf.inbox {
            Some(inbox) => Pushed::Inbox(inbox),
            None => Pushed::Refused(Box::new(pf)),
        });
    }

    let expected = remote_refs
        .get(name.as_ref())?
        .map_or(Expected::MustNotExist, Expected::MustExistAndMatch);
    advance(remote_refs, name, expected, local_tip)?;
    Ok(Pushed::Advanced(name.clone()))
}

/// Apply one ref advance as a single-edit CAS transaction.
fn advance(
    refs: &dyn RefStore,
    name: &FullName,
    expected: Expected,
    new: ObjectId,
) -> Result<TxOutcome> {
    Ok(refs.transaction(&[RefEdit {
        name: name.clone(),
        expected,
        new: Some(new),
    }])?)
}
