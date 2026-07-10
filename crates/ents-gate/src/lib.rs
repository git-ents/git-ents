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
//! - `gate.tip-signed`, `gate.refname-binding`, `gate.fast-forward` —
//!   [`verify`].
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
//! the inbox alternative ([`Refusal`]). `gate.adoption-no-cherry-pick`
//! is a prohibition on adoption *tooling* (`ents-sync`): a cherry-pick
//! produces an ordinary commit by the placer, which no pure function
//! over the result could distinguish, so the gate has nothing to check.
//!
//! # Authorization model
//!
//! "Signed by a member authorized for that refname" uses exactly the
//! rules the spec pins today: self-run namespaces are owner-only,
//! `refs/meta/effects/*` is admin-only (`effect.admin-only`), and
//! self-attested members are refused canonical refs until promoted
//! (`model.member-provenance`). Finer-grained, config-stored refname
//! rules (for example designating worker keys for one effect's results
//! namespace, `effect.official`) are a later, additive narrowing: they
//! arrive with a Config entity in `ents-model`, not a new gate.
//!
//! Signature-time semantics: a signature is judged against the member
//! entity in force *at the commit's own timestamp*, recovered by walking
//! the member ref's history (`model.member-revocation`). This keeps
//! verdicts reproducible after the fact in any clone — re-running the
//! gate years later returns the same answer — at the documented cost
//! that commit timestamps are author-controlled; the fast-forward
//! requirement still forces any replay to descend from the live tip.
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
//!     &refs, &objects, config_ref, &Config { epoch: Some(200) }, Some(&key), 200,
//! );
//!
//! // 3. From here on, every meta-ref update is judged by the tip invariant.
//! let issue = ents_model::Issue {
//!     title: "t".into(), body: "b".into(), state: "open".into(),
//!     assignees: vec![], labels: vec![],
//! };
//! let name: gix::refs::FullName = "refs/meta/issues/1".try_into().expect("valid");
//! let tip = write_meta_entity(&refs, &objects, name.clone(), &issue, Some(&key), 300);
//!
//! // The fixture already moved the ref; judge the same tip as a proposal
//! // against a pre-push copy of the store, the way pre-flight would.
//! let before = refs.fetched_copy();
//! before.remove(name.as_ref());
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
