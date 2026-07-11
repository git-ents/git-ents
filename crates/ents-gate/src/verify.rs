//! The pure verify function — the one admission judgment
//! (`gate.tip-signed` through `gate.fast-forward`, `gate.epoch`,
//! `gate.bootstrap`), identical at every call site (`gate.call-sites`).

use ents_model::namespace::{self, Namespace};
use ents_model::trailer::Trailers;
use ents_model::{Member, MemberId, MemberState, Provenance};
use gix::refs::FullName;
use gix_hash::ObjectId;
use gix_object::Find;
use gix_ref_store::{Expected, RefStoreRead};

use crate::config;
use crate::error::Result;
use crate::object::{CommitData, descends_from, read_commit};
use crate::policy;
use crate::signature;
use crate::verdict::{Admission, AdmissionKind, Refusal, Requirement, Verdict};

/// One proposed ref update, as every call site sees it: the refname and
/// the tip it should come to point at (`None` proposes deletion).
///
/// There is deliberately no `old` field: the gate reads the current tip
/// itself, from the same store snapshot its fast-forward check uses, and
/// returns it as the CAS precondition ([`Admission::cas`]) — binding
/// `gate.fast-forward` and `gate.atomic-cas` to one read.
///
/// # Examples
///
/// ```
/// use ents_gate::Update;
///
/// let update = Update {
///     name: "refs/meta/issues/42".try_into().expect("valid"),
///     new: Some(gix_hash::ObjectId::null(gix_hash::Kind::Sha1)),
/// };
/// assert!(update.new.is_some());
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Update {
    /// The ref being updated.
    pub name: FullName,
    /// The proposed new tip, or `None` to delete the ref.
    pub new: Option<ObjectId>,
}

/// Verify one proposed ref update against current repository state.
///
/// This is a pure function over the read half of the ref store and
/// gitoxide's object-find seam: same inputs, same verdict, no writes, no
/// clock, no transport state. The three call sites — hosted CAS
/// (`gate.mandatory-hosted`), local UI verdict (`gate.advisory-local`),
/// and push pre-flight (`sync.pre-flight`) — call exactly this function
/// (`gate.call-sites`) and differ only in what they do with a failing
/// verdict.
///
/// The checks, in spec order, for a `refs/meta/*` ref once the epoch is
/// in force (`gate.epoch`):
///
/// 1. `gate.tip-signed` — the new tip carries a `gpgsig` SSHSIG that
///    verifies against the key of an enrolled member whose entity,
///    *currently in force* at the member ref's tip in this same
///    snapshot, is active (`model.member-revocation`: acceptance-time
///    semantics — no commit-supplied timestamp participates) and whose
///    provenance authorizes this refname (`model.member-provenance`,
///    `effect.admin-only`).
/// 2. `gate.refname-binding` — the commit's `Ents-Ref:` trailer names
///    exactly this ref.
/// 3. `gate.fast-forward` — the new tip descends from the current tip.
///
/// Refs outside `refs/meta/*` pass as [`AdmissionKind::CodeRef`]: branch
/// refs keep transport-level authorization instead of the tip invariant
/// (`gate.principled-split`).
///
/// # Errors
///
/// An [`crate::Error`] means the gate could not evaluate (store or
/// object failure) — distinct from a [`Verdict::Fail`], which is a
/// reached judgment. The mandatory call site must treat both as
/// blocking.
///
/// # Examples
///
/// ```
/// use ents_gate::{AdmissionKind, Update, Verdict, verify};
/// use ents_testutil::{MemRefStore, ObjectStore};
///
/// let refs = MemRefStore::default();
/// let objects = ObjectStore::default();
///
/// // A code ref is not subject to the tip invariant.
/// let verdict = verify(&refs, &objects, &Update {
///     name: "refs/heads/main".try_into().expect("valid"),
///     new: Some(gix_hash::ObjectId::null(gix_hash::Kind::Sha1)),
/// }).expect("evaluates");
/// let Verdict::Pass(admission) = verdict else { panic!("code refs pass") };
/// assert_eq!(admission.kind, AdmissionKind::CodeRef);
/// ```
// @relation(gate.tip-signed, gate.refname-binding, gate.fast-forward, gate.atomic-cas, gate.epoch, gate.call-sites, gate.principled-split, scope=function)
pub fn verify(refs: &dyn RefStoreRead, objects: &dyn Find, update: &Update) -> Result<Verdict> {
    let old = refs.get(update.name.as_ref())?;
    let cas = old.map_or(Expected::MustNotExist, Expected::MustExistAndMatch);
    let pass = |kind: AdmissionKind| {
        Ok(Verdict::Pass(Admission {
            kind,
            refname: update.name.clone(),
            cas: cas.clone(),
        }))
    };

    // The principled split: content signatures authorize only
    // single-writer appends, which only meta-refs guarantee.
    // @relation(gate.principled-split, scope=function)
    if !update.name.as_bstr().starts_with(b"refs/meta/") {
        return pass(AdmissionKind::CodeRef);
    }

    // The verification epoch: the tip invariant applies only once an
    // epoch is recorded in refs/meta/config — or for the update that
    // records it, which must itself be the first gated tip of the
    // config ref (`gate.epoch`).
    let epoch = config::current_epoch(refs, objects)?;
    let epoch_setting = epoch.is_none()
        && update.name.as_bstr() == ents_model::namespace::CONFIG_REF
        && match update.new {
            // A new config that does not parse (or has no epoch) is not
            // epoch-setting; pre-epoch, it passes as archival anyway.
            Some(new) => matches!(config::epoch_at_commit(objects, new), Ok(Some(_))),
            None => false,
        };
    if epoch.is_none() && !epoch_setting {
        return pass(AdmissionKind::PreEpoch);
    }

    let refuse = |requirement: Requirement, detail: String, inbox_alternative: bool| {
        Ok(Verdict::Fail(Refusal {
            requirement,
            refname: update.name.clone(),
            detail,
            inbox_alternative,
        }))
    };

    // Meta-refs advance fast-forward-only; deletion is not a descent
    // and would discard the audit trail.
    let Some(new) = update.new else {
        return refuse(
            Requirement::FastForward,
            "meta-refs advance fast-forward-only; deletion is refused".into(),
            false,
        );
    };

    let Some(commit) = read_commit(objects, new)? else {
        return refuse(
            Requirement::TipSigned,
            "the proposed tip is not a commit object, so it cannot carry a member signature".into(),
            false,
        );
    };

    // gate.tip-signed / gate.signature-artifact: the signature is a data
    // artifact inside the commit object; no push certificate is read.
    // @relation(gate.signature-artifact, scope=function)
    let Some((payload, sig)) = signature::split_signed(&commit.raw) else {
        return refuse(
            Requirement::TipSigned,
            "the proposed tip is unsigned; meta-ref mutations must be author-signed commits".into(),
            false,
        );
    };

    let members = policy::members(refs)?;
    if members.is_empty() {
        return bootstrap(objects, update, new, &commit, &payload, &sig, old, &cas);
    }

    // Identify the signer: the member whose entity *currently in
    // force* — the member ref's tip in this same snapshot — carries the
    // verifying key (`model.member-revocation`). No commit-supplied
    // timestamp participates: a backdated committer date cannot reach
    // back past a revocation.
    let mut signer: Option<(MemberId, Member)> = None;
    for enrolled in &members {
        let member = policy::member_current(objects, enrolled.tip)?;
        if signature::verifies(&member.key, &payload, &sig) {
            signer = Some((enrolled.id.clone(), member));
            break;
        }
    }
    // A tip whose signature belongs to no authorized member is refused
    // even when it fast-forwards cleanly — which is exactly why
    // fast-forwarding a canonical ref directly to a contributor's
    // commit cannot be adoption (gate.adoption-no-fast-forward).
    // @relation(gate.tip-signed, gate.bootstrap, gate.adoption-no-fast-forward, scope=function)
    let Some((id, member)) = signer else {
        return refuse(
            Requirement::TipSigned,
            "the tip's signature does not verify against any member key currently enrolled".into(),
            false,
        );
    };

    // A revoked key authorizes no new pushes, full stop
    // (`model.member-revocation`): the judgment uses the member entity
    // currently in force, so a claimed pre-revocation committer
    // timestamp changes nothing. Refs the key placed before the
    // revocation landed stay valid — acceptance is never re-judged.
    if member.state == MemberState::Revoked {
        return refuse(
            Requirement::TipSigned,
            format!(
                "member {id}'s key is revoked; new pushes are refused regardless of the \
                 commit's claimed timestamp"
            ),
            false,
        );
    }

    if let Some(refusal) = authorize(&update.name, &id, &member) {
        return Ok(Verdict::Fail(refusal));
    }

    // gate.refname-binding: without this, a signed commit could be
    // replayed as the tip of a different meta-ref.
    // @relation(gate.refname-binding, scope=function)
    let trailers = Trailers::parse(&commit.message);
    if trailers.ents_ref.as_ref() != Some(&update.name) {
        let found = trailers
            .ents_ref
            .as_ref()
            .map_or_else(|| "no Ents-Ref trailer".to_owned(), |n| n.to_string());
        return refuse(
            Requirement::RefnameBinding,
            format!(
                "the commit was authored for {found}, not for {}",
                update.name.as_bstr()
            ),
            false,
        );
    }

    // gate.fast-forward: the parent hash is the anti-replay freshness
    // binding; the CAS precondition below pins the same old tip.
    // Descent through *any* parent suffices, and only the tip is
    // signature-checked — which is what makes an authorized member's
    // merge the adoption mechanism (gate.adoption-merge) and the
    // resolution for a member's own racing machines
    // (gate.same-actor-divergence).
    // @relation(gate.fast-forward, gate.adoption-merge, gate.same-actor-divergence, scope=function)
    if let Some(old) = old
        && !descends_from(objects, new, old)?
    {
        return refuse(
            Requirement::FastForward,
            "the new tip does not descend from the current tip; merge the divergent heads \
             (adoption and same-actor divergence are merges, never rewrites)"
                .into(),
            false,
        );
    }

    pass(AdmissionKind::TipInvariant)
}

/// Refname-keyed authorization for an identified, active signer
/// (`gate.tip-signed`'s "authorized for that refname").
///
/// The rules are exactly the ones the spec itself fixes:
///
/// - `refs/meta/self/<member>/*` is writable only by `<member>`
///   (`meta-ref.inbox`, `effect.self-run`).
/// - `refs/meta/effects/*` requires an admin-registered member
///   regardless of anything else (`effect.admin-only`).
/// - `refs/meta/inbox/<member>/*` is writable only by `<member>`, either
///   provenance — admins included may not write another member's
///   segment, because adoption is a merge onto the canonical ref, never
///   a write into the contributor's inbox (`meta-ref.inbox`).
/// - A self-attested member is not authorized for canonical refs — its
///   writes are limited to its own inbox and self-run namespaces
///   (`model.member-provenance`).
// @relation(gate.tip-signed, effect.admin-only, model.member-provenance, scope=function)
fn authorize(name: &FullName, id: &MemberId, member: &Member) -> Option<Refusal> {
    let namespace = namespace::classify(name.as_ref())?;
    let refuse = |detail: String, inbox_alternative: bool| {
        Some(Refusal {
            requirement: Requirement::TipSigned,
            refname: name.clone(),
            detail,
            inbox_alternative,
        })
    };
    match namespace {
        Namespace::SelfRun => {
            if namespace::self_run_owner(name.as_ref()).as_ref() == Some(id) {
                None
            } else {
                refuse(
                    format!(
                        "refs/meta/self/<member>/* is writable only by that member; \
                         {id} does not own this ref"
                    ),
                    false,
                )
            }
        }
        // The inbox is owner-keyed exactly like the self-run namespace:
        // a member — either provenance — writes only its own
        // refs/meta/inbox/<member>/* segment, and nobody, admins
        // included, writes another member's (meta-ref.inbox; adoption
        // is a merge onto the canonical ref, never a write into the
        // contributor's inbox). The legacy unscoped shape has no owner
        // segment, so it authorizes no one.
        Namespace::Inbox => {
            if namespace::inbox_owner(name.as_ref()).as_ref() == Some(id) {
                None
            } else {
                refuse(
                    format!(
                        "refs/meta/inbox/<member>/* is writable only by that member; \
                         {id} does not own this ref"
                    ),
                    false,
                )
            }
        }
        Namespace::Effect => match member.provenance {
            Provenance::AdminRegistered => None,
            Provenance::SelfAttested => refuse(
                format!(
                    "authoring an effect schedules code execution on canonical \
                     infrastructure; {id} is not admin-registered"
                ),
                true,
            ),
        },
        _ => match member.provenance {
            Provenance::AdminRegistered => None,
            Provenance::SelfAttested => refuse(
                format!(
                    "{id}'s membership is self-attested and not authorized for canonical \
                     refs until promoted by an admin-registered member"
                ),
                true,
            ),
        },
    }
}

/// The empty-member-list bootstrap window (`gate.bootstrap`): with no
/// `refs/meta/member/*` ref present, a first enrollment is
/// self-admitting — and only an enrollment. Self-admitting is taken
/// literally: the enrollment commit must be signed by the key inside the
/// Member tree it pushes, must bind to its refname, and must fast-forward,
/// so even the bootstrap write satisfies every mechanically-checkable
/// part of the tip invariant. Because this path is reachable only while
/// the member set is empty, a member set whose keys are all revoked
/// never reopens it: those updates take the ordinary path and fail
/// closed on the revoked state.
// @relation(gate.bootstrap, scope=function)
#[expect(
    clippy::too_many_arguments,
    reason = "a private continuation of verify(); grouping these into a struct would only rename the arguments"
)]
fn bootstrap(
    objects: &dyn Find,
    update: &Update,
    new: ObjectId,
    commit: &CommitData,
    payload: &[u8],
    sig: &str,
    old: Option<ObjectId>,
    cas: &Expected,
) -> Result<Verdict> {
    let refuse = |detail: String| {
        Ok(Verdict::Fail(Refusal {
            requirement: Requirement::TipSigned,
            refname: update.name.clone(),
            detail,
            inbox_alternative: false,
        }))
    };
    if namespace::classify(update.name.as_ref()) != Some(Namespace::Member) {
        return refuse(
            "no members are enrolled; only a first member enrollment is self-admitting".into(),
        );
    }
    let Ok(pushed) = facet_git_tree::deserialize::<Member>(&commit.tree, objects) else {
        return refuse("a first enrollment must push a readable Member entity".into());
    };
    if !signature::verifies(&pushed.key, payload, sig) {
        return refuse("a first enrollment must be signed by the key it enrolls".into());
    }
    let trailers = Trailers::parse(&commit.message);
    if trailers.ents_ref.as_ref() != Some(&update.name) {
        return Ok(Verdict::Fail(Refusal {
            requirement: Requirement::RefnameBinding,
            refname: update.name.clone(),
            detail: "the enrollment commit's Ents-Ref trailer does not name this ref".into(),
            inbox_alternative: false,
        }));
    }
    if let Some(old) = old
        && !descends_from(objects, new, old)?
    {
        return Ok(Verdict::Fail(Refusal {
            requirement: Requirement::FastForward,
            refname: update.name.clone(),
            detail: "the enrollment does not descend from the ref's current tip".into(),
            inbox_alternative: false,
        }));
    }
    Ok(Verdict::Pass(Admission {
        kind: AdmissionKind::Bootstrap,
        refname: update.name.clone(),
        cas: cas.clone(),
    }))
}
