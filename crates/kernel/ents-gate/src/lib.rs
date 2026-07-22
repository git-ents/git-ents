//! The gate: the one pure admission judgment over ref-store reads
//! (`docs/spec/gate.sdoc`).
//!
//! This crate owns exactly one verb — [`verify`] — evaluated identically
//! at the three call sites the design names (`gate.call-sites`): hosted
//! CAS (mandatory, a failing verdict aborts the transaction), local UI
//! verdict (advisory, a failing verdict annotates), and push pre-flight
//! (advisory, a prediction that can only go stale). It is deliberately a
//! separate crate from `receive` (`arch.gate-receive-split`) so the two
//! advisory call sites link no effect-matching or enqueue logic, and it
//! consumes only the *read* half of the ref store
//! (`arch.refstore-read-cas-split`) plus gitoxide's `Find` seam for
//! objects, so it is statically incapable of writing.
//!
//! # Spec coverage
//!
//! From `docs/spec/gate.sdoc`:
//!
//! - `gate.tip-signed`, `gate.identity-binding`, `gate.owner-mutation`,
//!   `gate.fast-forward` — [`verify`]. Identity binding recomputes each
//!   refname from the tip's signed content per namespace
//!   (`meta-ref.identity-binding`) — a natural-key tree field, a
//!   hash-identified genesis oid enforced by the all-roots walk, a
//!   composite review/result key, an inbox owner — and strictly decodes a
//!   genesis tree, an unknown entry refusing; owner mutation keys an
//!   advance to the genesis signer (∪ admins) or, for a review, the
//!   member the refname names.
//! - `gate.atomic-cas` — [`verify`] reads the old tip once and returns
//!   it as [`Admission::cas`], the precondition the writer MUST hand to
//!   `RefStore::transaction`; the CAS itself is the store's.
//! - `gate.signature-artifact` — signatures are read from the commit's
//!   `gpgsig` header and verified in-process; no push certificate is
//!   consulted, and no API here could accept one.
//! - `gate.policy-as-state` — members and the epoch are read only from
//!   `refs/meta/*` through `RefStoreRead`, so any clone evaluates the
//!   actual policy offline.
//! - `gate.epoch` — [`Config`]; the tip invariant applies once an epoch
//!   is recorded, and the epoch-setting commit is itself the first gated
//!   tip of the config ref.
//! - `gate.call-sites`, `gate.verdict-reason` — [`Verdict`], [`Refusal`],
//!   [`Requirement`]; proven identical across call sites by this crate's
//!   parameterized call-site test.
//! - `gate.adoption-merge`, `gate.adoption-no-fast-forward`,
//!   `gate.same-actor-divergence` — consequences of judging only the tip
//!   plus DAG descent; pinned by the verdict-table tests.
//! - `gate.principled-split` — refs outside `refs/meta/*` pass as
//!   [`AdmissionKind::CodeRef`]; the tip invariant never applies to
//!   branch refs.
//! - `gate.bootstrap` — the empty-member-list window admits only a
//!   self-admitting first enrollment; an all-revoked member set fails
//!   closed and never reopens it.
//!
//! Partially here, completed by later phases: `gate.mandatory-hosted`
//! and `gate.advisory-local` are caller policies (`ents-receive`, the
//! composition roots) — this crate contributes the shared verdict and,
//! for the advisory sites, the verdict-time reason rendering including
//! the inbox alternative ([`Refusal`]). The prohibition on cherry-pick
//! as an adoption mechanism binds the adoption *tooling*
//! (`sync.adoption-no-cherry-pick`, enforced by `ents-sync`): a
//! cherry-pick produces an ordinary commit by the placer, which no pure
//! function over the result could distinguish, so the gate has nothing
//! to check — gate.sdoc's Adoption section says the same.
//!
//! # Authorization model
//!
//! "Signed by a member authorized for that refname" uses exactly the
//! rules the spec pins today: self-run and inbox namespaces are
//! owner-only — a member of either provenance writes only its own
//! `refs/meta/self/<member>/*` and `refs/meta/inbox/<member>/*`
//! segments, and nobody, admins included, writes another member's
//! (`meta-ref.inbox`) — `refs/meta/effects/*` is admin-only
//! (`effect.admin-only`), and self-attested members are refused
//! canonical refs until promoted (`model.member-provenance`).
//! Finer-grained, config-stored refname rules are a later, additive
//! narrowing read from the same [`Config`] the epoch already lives on
//! (`Config::workers`): `docs/agent-sessions-plan.adoc`'s Phase 2a is the
//! first one, admitting a designated worker to advance any member's agent
//! session (∪ genesis signer, ∪ admins) — `verify::owner_mutation`'s
//! `Namespace::AgentSession` arm. Designating worker keys for one effect's
//! canonical results namespace (`effect.official`) is the same roster,
//! unbuilt until a caller needs it; `Config` itself moves to `ents-model`
//! only once configuration grows fields no gate rule reads.
//!
//! Acceptance-time semantics: a signature is judged against the member
//! entity *currently in force* — the member ref's tip in the same
//! snapshot the gate reads (`model.member-revocation`). No
//! commit-supplied timestamp participates, so a revoked key's new
//! pushes are refused even with a backdated committer date; refs
//! accepted before the revocation stay valid because acceptance is
//! never re-judged. A verdict is therefore a pure function of the
//! proposed update and current repository state: any clone reproduces
//! it against the same snapshot, and reconstructing what a *past*
//! acceptance saw is an audit function over the deployment's op log —
//! explicitly out of scope for this crate and every other crate here.
//!
//! # Examples
//!
//! A hosted-shaped round trip: enroll a member (pre-epoch, archival),
//! turn verification on by setting the epoch (the first gated tip of the
//! config ref), then verify a signed mutation and use the admission's
//! CAS precondition.
//!
//! ```
//! use ents_gate::{AdmissionKind, Config, Update, Verdict, verify};
//! use ents_model::{Provenance, namespace};
//! use ents_testutil::{Keypair, MemRefStore, ObjectStore, enroll_member, write_meta_entity};
//! use gix_ref_store::Expected;
//!
//! let refs = MemRefStore::default();
//! let objects = ObjectStore::default();
//! let key = Keypair::from_seed(1);
//!
//! // 1. Enrollment lands pre-epoch: history before the epoch is archival.
//! enroll_member(&refs, &objects, "jdc", &key, Provenance::AdminRegistered, 100);
//!
//! // 2. The epoch-setting commit is the first gated tip of refs/meta/config.
//! let config_ref: gix::refs::FullName = namespace::CONFIG_REF.try_into().expect("valid");
//! let epoch_tip = write_meta_entity(
//!     &refs, &objects, config_ref, &Config { epoch: Some(200), ..Config::default() }, Some(&key), 200,
//! );
//!
//! // 3. From here on, every meta-ref update is judged by the tip invariant.
//! // (A stand-in for `ents-forge`'s `Issue`: this crate cannot depend on
//! // `ents-forge`, which itself depends on this crate.)
//! # #[derive(facet::Facet)]
//! # struct Issue { title: String, body: String, state: String }
//! #
//! let issue = Issue {
//!     title: "t".into(), body: "b".into(), state: "open".into(),
//! };
//! // A hash-identified entity's id is its genesis commit's own oid, so the
//! // ref is named from the signed commit (`meta-ref.identity-binding`).
//! let tree = facet_git_tree::serialize_into(&issue, &objects).expect("serializes");
//! let tip = ents_testutil::write_commit(&objects, &ents_testutil::CommitSpec {
//!     tree, parents: vec![], message: "Open an issue".into(), seconds: 300,
//! }, Some(&key));
//! let name: gix::refs::FullName = format!("refs/meta/issues/{tip}").try_into().expect("valid");
//!
//! // Judge the tip as a proposal against a store that does not yet have
//! // the ref, the way hosted CAS or pre-flight would.
//! let before = refs.fetched_copy();
//! let verdict = verify(&before, &objects, &Update { name, new: Some(tip) })
//!     .expect("evaluates");
//! let Verdict::Pass(admission) = verdict else { panic!("authorized update passes") };
//! assert_eq!(admission.kind, AdmissionKind::TipInvariant);
//! assert_eq!(admission.cas, Expected::MustNotExist);
//! # let _ = epoch_tip;
//! ```

mod config;
mod error;
mod object;
mod policy;
mod signature;
mod verdict;
mod verify;

pub use config::Config;
pub use error::{Error, Result};
pub use verdict::{Admission, AdmissionKind, Refusal, Requirement, Verdict};
pub use verify::{Update, verify};
