//! The pure verify function — the one admission judgment
//! (`gate.tip-signed` through `gate.fast-forward`, `gate.epoch`,
//! `gate.bootstrap`), identical at every call site (`gate.call-sites`).

use ents_model::namespace::{self, Namespace};
use ents_model::{Member, MemberId, MemberState, Provenance, ResultRecord};
use facet::{Facet, Type, UserType};
use gix::refs::FullName;
use gix_hash::ObjectId;
use gix_object::Find;
use gix_ref_store::{Expected, RefStoreRead};

use crate::config;
use crate::error::Result;
use crate::object::{
    CommitData, all_roots, descends_from, read_commit, read_tree_entry, tree_entry_names,
};
use crate::policy::{self, Enrolled};
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
/// 2. `gate.identity-binding` — the refname is recomputed from the
///    proposed tip's signed content, per namespace exactly as
///    `meta-ref.identity-binding` tabulates (a natural-key tree field, a
///    hash-identified genesis oid enforced by the all-roots walk, a
///    composite review/result key, an inbox owner), and must match
///    `update.name`; a hash-identified or composite genesis additionally
///    strictly decodes as its entity type, an unknown tree entry
///    refusing.
/// 3. `gate.owner-mutation` — a hash-identified entity's ref advances
///    only under its genesis signer (∪ admins); a review advances only
///    under the member its refname names. Creation stays provenance-keyed.
/// 4. `gate.fast-forward` — the new tip descends from the current tip.
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
// @relation(gate.tip-signed, gate.identity-binding, gate.owner-mutation, gate.fast-forward, gate.atomic-cas, gate.epoch, gate.call-sites, gate.principled-split, scope=function)
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

    // gate.identity-binding: recompute the refname from the tip's signed
    // content per namespace and refuse a mismatch — a signed commit
    // cannot be replayed as the tip of a different meta-ref than the one
    // its content names. This is what the retired Advance-ref trailer
    // used to assert by side channel; now the refname is a total function
    // of the tree, the genesis oid, and the signer.
    // @relation(gate.identity-binding, scope=function)
    if let Some(refusal) = identity_binding(objects, &update.name, new, &commit, &id)? {
        return Ok(Verdict::Fail(refusal));
    }

    // gate.owner-mutation: a hash-identified entity's ref advances only
    // under its genesis signer (∪ admins); a review advances only under
    // the member its refname names. Creation stays provenance-keyed,
    // already judged by `authorize` above.
    // @relation(gate.owner-mutation, scope=function)
    if let Some(refusal) =
        owner_mutation(objects, &update.name, old, new, &members, &id, &member)?
    {
        return Ok(Verdict::Fail(refusal));
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
    // The same natural-key identity binding as the ordinary path
    // (`gate.identity-binding`, `model.member-identity`): the enrolled
    // member's own id field must recompute the refname being written, so
    // even a bootstrap write names its ref from signed content, not a
    // trailer.
    // @relation(gate.identity-binding, scope=function)
    let bound = namespace::member_ref(&pushed.id).ok();
    if bound.as_ref() != Some(&update.name) {
        return Ok(Verdict::Fail(Refusal {
            requirement: Requirement::IdentityBinding,
            refname: update.name.clone(),
            detail: format!(
                "the enrollment's id field is {}, which names {}, not {}",
                pushed.id,
                bound.map_or_else(|| "an invalid ref".to_owned(), |n| n.as_bstr().to_string()),
                update.name.as_bstr()
            ),
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

/// Build an [`Requirement::IdentityBinding`] refusal for `name`.
fn binding_refusal(name: &FullName, detail: String) -> Option<Refusal> {
    Some(Refusal {
        requirement: Requirement::IdentityBinding,
        refname: name.clone(),
        detail,
        inbox_alternative: false,
    })
}

/// The value of a scalar tree field as UTF-8, or `None` if the entry is
/// absent or not valid UTF-8.
fn field_str(objects: &dyn Find, tree: ObjectId, field: &str) -> Result<Option<String>> {
    Ok(read_tree_entry(objects, tree, field)?
        .and_then(|bytes| String::from_utf8(bytes).ok()))
}

/// The hex form of a raw-oid (`[u8; 20]`) tree field, or `None` when the
/// entry is absent or not 20 bytes.
fn field_oid_hex(objects: &dyn Find, tree: ObjectId, field: &str) -> Result<Option<String>> {
    Ok(read_tree_entry(objects, tree, field)?.and_then(|bytes| {
        (bytes.len() == 20).then(|| ObjectId::from_bytes_or_panic(&bytes).to_string())
    }))
}

/// The final `/`-delimited segment of a refname.
fn final_segment(name: &FullName) -> String {
    name.as_bstr()
        .to_string()
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_owned()
}

/// A natural-key binding: the tree field `field` must equal `expected`
/// (the refname's final segment).
fn bind_natural_key(
    objects: &dyn Find,
    name: &FullName,
    tree: ObjectId,
    field: &str,
    expected: &str,
) -> Result<Option<Refusal>> {
    match field_str(objects, tree, field)? {
        Some(value) if value == expected => Ok(None),
        Some(value) => Ok(binding_refusal(
            name,
            format!(
                "the tree's `{field}` field is `{value}`, which names a different ref than {}",
                name.as_bstr()
            ),
        )),
        None => Ok(binding_refusal(
            name,
            format!("the tree carries no `{field}` field to bind {}", name.as_bstr()),
        )),
    }
}

/// A hash-identified binding: the refname's final segment must be the
/// genesis commit's oid, and every parentless commit reachable from the
/// proposed tip must be that genesis (`meta-ref.identity-binding`'s
/// all-roots rule). This, not a creation-time-only check, is what refuses
/// replaying a signed mutation commit as a doppelgänger genesis.
fn bind_hash_identified(
    objects: &dyn Find,
    name: &FullName,
    new: ObjectId,
) -> Result<Option<Refusal>> {
    let segment = final_segment(name);
    let Ok(expected) = ObjectId::from_hex(segment.as_bytes()) else {
        return Ok(binding_refusal(
            name,
            format!("`{segment}` is not a genesis commit oid"),
        ));
    };
    let roots = all_roots(objects, new)?;
    if roots.is_empty() {
        return Ok(binding_refusal(
            name,
            "the proposed tip has no readable genesis root to bind its id".into(),
        ));
    }
    for root in roots {
        if root != expected {
            return Ok(binding_refusal(
                name,
                format!(
                    "a parentless commit {root} reachable from the proposed tip is not the \
                     genesis {expected} the refname names — a signed mutation cannot be replayed \
                     as a new entity's genesis"
                ),
            ));
        }
    }
    Ok(None)
}

/// Reject a genesis tree that carries an entry which is not a field of its
/// namespace's entity type, or that does not decode as that type at all
/// (`gate.identity-binding`: strict genesis decode — the pairwise-disjoint
/// structs, held by test, are what let this stand in for a stored
/// `.schema` marker).
fn strict_decode<T: for<'facet> Facet<'facet>>(
    objects: &dyn Find,
    name: &FullName,
    tree: ObjectId,
) -> Result<Option<Refusal>> {
    let Type::User(UserType::Struct(st)) = T::SHAPE.ty else {
        return Ok(None);
    };
    let fields: Vec<&str> = st.fields.iter().map(|f| f.name).collect();
    for entry in tree_entry_names(objects, tree)? {
        if !fields.contains(&entry.as_str()) {
            return Ok(binding_refusal(
                name,
                format!(
                    "the genesis tree carries an unknown entry `{entry}` for a \
                     {} entity; strict decode refuses it",
                    T::SHAPE.type_identifier
                ),
            ));
        }
    }
    if facet_git_tree::deserialize::<T>(&tree, objects).is_err() {
        return Ok(binding_refusal(
            name,
            format!(
                "the genesis tree does not decode as a {} entity",
                T::SHAPE.type_identifier
            ),
        ));
    }
    Ok(None)
}

/// Recompute `name` from the proposed tip's signed content, per namespace
/// exactly as `meta-ref.identity-binding` tabulates, returning a refusal
/// on mismatch (`gate.identity-binding`). `None` means the binding holds.
///
/// The recomputation reads binding fields by tree-entry name generically
/// (`read_tree_entry`), so it never depends on a non-kernel entity crate
/// to bind a review's target or a toolchain's name; the one exception is
/// strict genesis decode of a result, whose type this crate owns.
// @relation(gate.identity-binding, meta-ref.identity-binding, scope=function)
fn identity_binding(
    objects: &dyn Find,
    name: &FullName,
    new: ObjectId,
    commit: &CommitData,
    signer: &MemberId,
) -> Result<Option<Refusal>> {
    let Some(namespace) = namespace::classify(name.as_ref()) else {
        return Ok(None);
    };
    let is_genesis = commit.parents.is_empty();
    match namespace {
        // Singleton state binds by its fixed name, which `classify`
        // already established, and an unknown namespace cannot be bound by
        // a vocabulary that does not know it (`model.extensibility`).
        Namespace::Account | Namespace::Config | Namespace::Unknown => Ok(None),
        Namespace::Member => {
            bind_natural_key(objects, name, commit.tree, "id", &final_segment(name))
        }
        Namespace::Effect | Namespace::Toolchain => {
            bind_natural_key(objects, name, commit.tree, "name", &final_segment(name))
        }
        Namespace::Comment | Namespace::Issue => bind_hash_identified(objects, name, new),
        Namespace::Review => {
            let Some((target, member)) = namespace::parse_review_ref(name.as_ref()) else {
                return Ok(binding_refusal(
                    name,
                    "not a well-formed reviews/<target>/<member> refname".into(),
                ));
            };
            match field_oid_hex(objects, commit.tree, "target")? {
                Some(hex) if hex == target => {}
                other => {
                    return Ok(binding_refusal(
                        name,
                        format!(
                            "the review's target field {} does not name the reviewed commit \
                             {target} in its refname",
                            other.unwrap_or_else(|| "(absent)".into())
                        ),
                    ));
                }
            }
            if signer.as_str() != member.as_str() {
                return Ok(binding_refusal(
                    name,
                    format!(
                        "the review is signed by {signer}, not the reviewer {member} its \
                         refname names"
                    ),
                ));
            }
            Ok(None)
        }
        Namespace::Result | Namespace::SelfRun => {
            let Some((effect, short_oid)) = namespace::parse_result_ref(name.as_ref()) else {
                return Ok(binding_refusal(
                    name,
                    "not a well-formed results/<effect>/<short-oid> refname".into(),
                ));
            };
            match field_str(objects, commit.tree, "effect")? {
                Some(value) if value == effect => {}
                other => {
                    return Ok(binding_refusal(
                        name,
                        format!(
                            "the result's effect field {} does not name {effect} in its refname",
                            other.unwrap_or_else(|| "(absent)".into())
                        ),
                    ));
                }
            }
            match field_oid_hex(objects, commit.tree, "target")? {
                Some(hex) if hex.starts_with(&short_oid) => {}
                other => {
                    return Ok(binding_refusal(
                        name,
                        format!(
                            "the result's target field {} does not begin with the short oid \
                             {short_oid} in its refname",
                            other.unwrap_or_else(|| "(absent)".into())
                        ),
                    ));
                }
            }
            if namespace == Namespace::SelfRun
                && namespace::self_run_owner(name.as_ref()).as_ref() != Some(signer)
            {
                return Ok(binding_refusal(
                    name,
                    format!("a self-run result under refs/meta/self/* must be signed by {signer}"),
                ));
            }
            if is_genesis
                && let Some(refusal) = strict_decode::<ResultRecord>(objects, name, commit.tree)?
            {
                return Ok(Some(refusal));
            }
            Ok(None)
        }
        // A pin mirrors its entity's segments by construction and carries
        // the empty tree; its ancestry deliberately reaches into code
        // history, so the all-roots walk is NEVER applied to it
        // (`meta-ref.identity-binding`).
        Namespace::Pin => Ok(None),
        // An inbox ref binds by its owner segment equal to the signer
        // (already enforced by `authorize`), with the canonical suffix
        // bound exactly as its canonical namespace binds — recurse on the
        // synthesized canonical refname.
        Namespace::Inbox => {
            if namespace::inbox_owner(name.as_ref()).as_ref() != Some(signer) {
                return Ok(binding_refusal(
                    name,
                    format!("an inbox ref's owner segment must equal its signer {signer}"),
                ));
            }
            let path = name.as_bstr().to_string();
            let Some(rest) = path.strip_prefix("refs/meta/inbox/") else {
                return Ok(None);
            };
            let Some((_, suffix)) = rest.split_once('/') else {
                return Ok(None);
            };
            let Ok(canonical) = FullName::try_from(format!("refs/meta/{suffix}")) else {
                return Ok(None);
            };
            identity_binding(objects, &canonical, new, commit, signer)
        }
        // `Namespace` is `#[non_exhaustive]`; a variant this build does
        // not know is treated like `Unknown` — unbindable by a vocabulary
        // that cannot interpret it (`model.extensibility`).
        _ => Ok(None),
    }
}

/// Ownership keys mutation (`gate.owner-mutation`): a hash-identified
/// entity's ref advances only under its genesis signer or an
/// admin-registered member; a review advances only under the member its
/// refname names. Creation stays provenance-keyed (judged by `authorize`),
/// so this fires only on an advance.
// @relation(gate.owner-mutation, scope=function)
fn owner_mutation(
    objects: &dyn Find,
    name: &FullName,
    old: Option<ObjectId>,
    new: ObjectId,
    members: &[Enrolled],
    signer_id: &MemberId,
    signer: &Member,
) -> Result<Option<Refusal>> {
    let Some(namespace) = namespace::classify(name.as_ref()) else {
        return Ok(None);
    };
    let refuse = |detail: String| {
        Some(Refusal {
            requirement: Requirement::TipSigned,
            refname: name.clone(),
            detail,
            inbox_alternative: false,
        })
    };
    let is_admin = signer.provenance == Provenance::AdminRegistered;
    match namespace {
        Namespace::Comment | Namespace::Issue => {
            // Creation is provenance-keyed; only an advance is owner-keyed.
            if old.is_none() {
                return Ok(None);
            }
            if is_admin {
                return Ok(None);
            }
            let genesis = all_roots(objects, new)?;
            let genesis_signer = match genesis.first() {
                Some(root) => commit_signer(objects, members, *root)?,
                None => None,
            };
            if genesis_signer.as_ref() == Some(signer_id) {
                Ok(None)
            } else {
                Ok(refuse(format!(
                    "{signer_id} is neither the member whose signature this entity's genesis \
                     carries nor an admin-registered member, so may not advance {}",
                    name.as_bstr()
                )))
            }
        }
        Namespace::Review => {
            let Some((_, member)) = namespace::parse_review_ref(name.as_ref()) else {
                return Ok(None);
            };
            if signer_id.as_str() == member.as_str() {
                Ok(None)
            } else {
                Ok(refuse(format!(
                    "a review advances only under the signature of {member}, the reviewer its \
                     refname names, not {signer_id}"
                )))
            }
        }
        _ => Ok(None),
    }
}

/// The enrolled member whose currently-in-force key signed `oid`, or
/// `None` when the commit is unsigned or signed by no enrolled member —
/// used to recover a hash-identified entity's genesis signer
/// (`gate.owner-mutation`).
fn commit_signer(
    objects: &dyn Find,
    members: &[Enrolled],
    oid: ObjectId,
) -> Result<Option<MemberId>> {
    let Some(commit) = read_commit(objects, oid)? else {
        return Ok(None);
    };
    let Some((payload, sig)) = signature::split_signed(&commit.raw) else {
        return Ok(None);
    };
    for enrolled in members {
        let member = policy::member_current(objects, enrolled.tip)?;
        if signature::verifies(&member.key, &payload, &sig) {
            return Ok(Some(enrolled.id.clone()));
        }
    }
    Ok(None)
}
