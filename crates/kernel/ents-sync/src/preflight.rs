//! Push pre-flight and inbox routing — turning the gate's verdict into a
//! decision the user acts on before pushing.
//!
//! Two requirements live here. [`preflight`] runs the *identical* gate
//! function every other call site runs ([`ents_gate::verify`],
//! `gate.call-sites`), so a pre-flight verdict is a prediction that can
//! only go stale between pre-flight and the hosted CAS, never one that is
//! wrong about the rules (`sync.pre-flight`). And [`inbox_route`] computes
//! the alternative a negative verdict offers: the same commit re-homed
//! under the author's own `refs/meta/inbox/<member>/*` segment, awaiting
//! adoption (`sync.inbox-routing`).
//!
//! The offer is a function of the verdict alone, so it is available the
//! moment the verdict goes negative — at all three advisory sites the spec
//! names (the local UI verdict at commit time, push pre-flight, and the
//! canonical store's actual rejection, `sync.inbox-routing`) — not only
//! after a push is attempted and refused. Sync never blocks a *local*
//! write on a failing verdict (`sync.local-advisory`); the consequence it
//! owns is exactly this inbox offer.

use ents_gate::{Update, Verdict, verify};
use ents_model::{MemberId, namespace};
use gix::refs::{FullName, FullNameRef};
use gix_object::Find;
use gix_ref_store::RefStoreRead;

use crate::error::Result;

/// A pre-flight prediction: the gate's verdict on a proposed update, plus
/// the inbox alternative a negative verdict offers (`sync.pre-flight`,
/// `sync.inbox-routing`).
///
/// `verdict` is produced by the same [`ents_gate::verify`] the hosted store
/// runs at CAS time, so it predicts that outcome exactly up to staleness of
/// the last fetch — it cannot disagree about the rules (`gate.call-sites`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreFlight {
    /// The gate's verdict on the proposed update.
    pub verdict: Verdict,
    /// Where the same commit could instead be routed — the author's own
    /// inbox ref — when the verdict is a negative one the inbox can absorb
    /// (`sync.inbox-routing`). `None` on a pass, and on refusals the inbox
    /// cannot help (a divergence, whose answer is a merge, or a refname
    /// mismatch).
    pub inbox: Option<FullName>,
}

impl PreFlight {
    /// Whether the gate admits the update. A caller pushing to a hosted
    /// store treats a false here as a prediction the push will be refused
    /// — never as authority to block a *local* write (`sync.local-advisory`):
    /// this type carries no write access at all, so a failing verdict is
    /// structurally incapable of vetoing one; the rejection consequence
    /// sync owns instead is [`PreFlight::inbox`] (`sync.inbox-routing`).
    // @relation(sync.local-advisory, scope=function)
    #[must_use]
    pub fn is_pass(&self) -> bool {
        self.verdict.is_pass()
    }
}

/// Evaluate push pre-flight for one proposed update (`sync.pre-flight`).
///
/// Runs the identical gate function the hosted store runs
/// (`gate.call-sites`) against the local (last-fetched) snapshot, and, when
/// the verdict is a refusal the inbox can absorb, attaches the route the
/// author would use instead (`sync.inbox-routing`). `author` is the member
/// whose inbox segment such a routed commit would land under — its own, and
/// only its own (`meta-ref.inbox`).
///
/// # Errors
///
/// Propagates [`ents_gate::Error`] when the gate cannot evaluate (a store
/// or object read failed) and a refname error if the inbox route cannot be
/// built.
///
/// # Examples
///
/// ```
/// use ents_model::{MemberId, Provenance, namespace};
/// use ents_sync::preflight::preflight;
/// use ents_gate::Update;
/// use ents_testutil::{Keypair, MemRefStore, ObjectStore, enroll_member, write_meta_entity};
///
/// // A stand-in for `ents-forge`'s `Issue` (this crate cannot depend on
/// // `ents-forge`): any Facet-derived entity exercises pre-flight.
/// # #[derive(facet::Facet)]
/// # struct Issue { title: String, body: String, state: String }
/// #
/// let refs = MemRefStore::default();
/// let objects = ObjectStore::default();
/// let admin = Keypair::from_seed(1);
/// enroll_member(&refs, &objects, "admin", &admin, Provenance::AdminRegistered, 100);
/// let config: gix::refs::FullName = namespace::CONFIG_REF.try_into().expect("valid");
/// write_meta_entity(&refs, &objects, config, &ents_gate::Config { epoch: Some(200) }, Some(&admin), 200);
///
/// // A self-attested contributor's canonical issue push fails pre-flight,
/// // and the inbox route is offered at once.
/// let bob = Keypair::from_seed(2);
/// enroll_member(&refs, &objects, "bob", &bob, Provenance::SelfAttested, 250);
/// let name: gix::refs::FullName = "refs/meta/issues/9".try_into().expect("valid");
/// let issue = Issue { title: "t".into(), body: "b".into(), state: "open".into() };
/// let tip = write_meta_entity(&refs, &objects, name.clone(), &issue, Some(&bob), 300);
///
/// let before = refs.fetched_copy();
/// before.remove(name.as_ref());
/// let pf = preflight(&before, &objects, &Update { name, new: Some(tip) }, &MemberId::new("bob")).expect("evaluates");
/// assert!(!pf.is_pass());
/// assert_eq!(pf.inbox.expect("offered").as_bstr(), "refs/meta/inbox/bob/issues/9");
/// ```
// @relation(sync.pre-flight, sync.inbox-routing, scope=function)
pub fn preflight(
    refs: &dyn RefStoreRead,
    objects: &dyn Find,
    update: &Update,
    author: &MemberId,
) -> Result<PreFlight> {
    let verdict = verify(refs, objects, update)?;
    let inbox = match &verdict {
        // The gate already decided whether the inbox is the alternative:
        // `inbox_alternative` is set exactly on authorization refusals
        // against a canonical ref, and cleared for divergences (answer: a
        // merge) and refname mismatches (`gate.verdict-reason`,
        // `gate.advisory-local`).
        Verdict::Fail(refusal) if refusal.inbox_alternative => {
            Some(inbox_route(update.name.as_ref(), author)?)
        }
        _ => None,
    };
    Ok(PreFlight { verdict, inbox })
}

/// The inbox ref a rejected commit against `canonical` would be routed to,
/// under `author`'s own segment (`sync.inbox-routing`, `meta-ref.inbox`).
///
/// The canonical ref's suffix below `refs/meta/` becomes the inbox id, so
/// `refs/meta/issues/42` routes to `refs/meta/inbox/<author>/issues/42`:
/// the destination records both who is submitting and what they submit,
/// and stays under the author's own segment, the only place a member may
/// write (`meta-ref.inbox`). A refname already outside `refs/meta/`, or an
/// already-inbox ref, is returned unchanged — there is nothing to re-route.
///
/// # Errors
///
/// Propagates a refname error if the composed inbox refname is invalid.
///
/// # Examples
///
/// ```
/// use ents_model::MemberId;
/// use ents_sync::preflight::inbox_route;
///
/// let canonical: gix::refs::FullName = "refs/meta/issues/42".try_into().expect("valid");
/// let routed = inbox_route(canonical.as_ref(), &MemberId::new("jdc")).expect("valid");
/// assert_eq!(routed.as_bstr(), "refs/meta/inbox/jdc/issues/42");
/// ```
// @relation(sync.inbox-routing, scope=function)
pub fn inbox_route(canonical: &FullNameRef, author: &MemberId) -> Result<FullName> {
    if namespace::is_inbox(canonical) {
        return Ok(canonical.to_owned());
    }
    let path = canonical.as_bstr().to_string();
    let Some(suffix) = path.strip_prefix("refs/meta/") else {
        return Ok(canonical.to_owned());
    };
    Ok(namespace::inbox_ref(author, suffix)?)
}
