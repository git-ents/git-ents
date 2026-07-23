//! `receive`: the one write path every mutation frontend shares
//! (`docs/spec/receive.adoc`).
//!
//! This crate's single responsibility is orchestration above traits that
//! already exist by the time it lands: gate policy (mandatory hosted,
//! advisory local), redaction enforcement at ingest, and effect-footprint
//! matching plus enqueue — never the gate's own judgment (`ents-gate`),
//! never the query algebra (`ents-query`), and never an executor
//! (`ents-effect`, a later phase this crate must never link,
//! `arch.query-effect-split`).
//!
//! # Spec coverage
//!
//! From `docs/spec/receive.adoc`:
//!
//! - `receive.unit`, `receive.shared-path` — [`receive`]: the sole
//!   mutation entry point, identical for every frontend; only the trait
//!   implementations and [`Mode`] differ.
//! - `receive.proposal-shape` — [`Proposal`], [`RefTransition`],
//!   [`TransportAuth`].
//! - `receive.refstore-seam` — [`receive`] takes `&dyn RefStore`, the full
//!   read/CAS seam (`arch.refstore-read-cas-split`).
//! - `receive.object-access` — object access uses only `gix_object::Find`
//!   and `gix_object::Write`; see [`receive`]'s own doc for the one
//!   deliberate deviation (`gix_object::Exists` omitted — a fixture gap,
//!   not a design choice) and for the quarantine-directory note.
//! - `receive.event-sink`, `receive.never-blocks` — [`EventSink`]; enqueue
//!   is the entire synchronous cost `receive` adds, and it is computed via
//!   each effect's static footprint, never a re-scan of every effect on
//!   every push.
//! - `receive.dedup` — [`MemoryEventSink`]'s `(effect, oid)` set.
//! - `receive.reconstructible` — [`reconcile`], the boot-time scan that
//!   rebuilds the exact obligations incremental `receive` calls would have
//!   enqueued, from repository state alone (`query.workset`).
//! - `receive.redaction-admin-only` — a consequence of composition, not new
//!   code: `refs/meta/redactions/*` already falls through `ents-gate`'s
//!   default authorization arm, which requires admin-registered provenance
//!   for every namespace without its own carve-out; this crate's own test
//!   suite pins that composition at the `receive` level.
//! - `receive.redaction-ingest` — [`receive`]'s first step: any proposal
//!   object matching a recorded redaction target refuses the whole batch.
//!
//! [`propose_entity`], [`propose_genesis`], and [`propose_delete`] are the
//! shared mechanism every entity-mutation frontend builds its call to
//! [`receive`] through: they serialize a typed tree and hand a signed
//! commit to `receive`, whose ref name the gate recomputes from the signed
//! content (`meta-ref.identity-binding`) — no commit trailer binds it. An
//! owner-keyed mutation advances a known ref ([`propose_entity`]); the
//! creation of a hash-identified entity names its ref from the genesis
//! commit's own oid ([`propose_genesis`]). One place signs, one place
//! calls `receive`, shared by `git-ents`'s `members`, `account`, `effect`,
//! `toolchain`, `comment`, and `redact` commands (and, later,
//! `ents-forge`'s own comment command) alike.
//!
//! # Examples
//!
//! An end-to-end local write path: advisory gate, null sink — the shape
//! `receive.adoc`'s phase-4 exit criterion runs.
//!
//! ```
//! use ents_gate::Config;
//! use ents_model::{Provenance, namespace};
//! use ents_receive::{Mode, NullEventSink, Proposal, RefTransition, TxResult, receive};
//! use ents_testutil::{Keypair, MemRefStore, ObjectStore, enroll_member, write_meta_entity};
//!
//! let refs = MemRefStore::default();
//! let objects = ObjectStore::default();
//! let admin = Keypair::from_seed(1);
//!
//! enroll_member(&refs, &objects, "admin", &admin, Provenance::AdminRegistered, 100);
//! let config_ref: gix::refs::FullName = namespace::CONFIG_REF.try_into().expect("valid");
//! let tip = write_meta_entity(
//!     &refs, &objects, config_ref.clone(), &Config { epoch: Some(200) }, Some(&admin), 200,
//! );
//!
//! // The fixture already moved the ref; re-propose the same tip through
//! // `receive` against a pre-write copy, the way a CLI would.
//! let before = refs.fetched_copy();
//! before.remove(config_ref.as_ref());
//! let proposal = Proposal {
//!     transitions: vec![RefTransition { name: config_ref, old: None, new: Some(tip) }],
//!     objects: vec![tip],
//!     auth: None,
//! };
//!
//! let outcome = receive(&before, &objects, &NullEventSink, &proposal, Mode::Advisory)
//!     .expect("evaluates");
//! assert_eq!(outcome.result, TxResult::Applied);
//! ```

mod error;
mod outcome;
mod proposal;
mod propose;
mod receive;
mod reconcile;
mod sink;

pub use error::{Error, Result};
pub use outcome::{Mode, Outcome, TxResult};
pub use proposal::{Proposal, RefTransition, TransportAuth};
pub use propose::{
    Identity, entity_transition, propose_delete, propose_entity, propose_entity_with_pin,
    propose_genesis, propose_genesis_retaining, propose_pin,
};
pub use receive::receive;
pub use reconcile::reconcile;
pub use sink::{EventSink, MemoryEventSink, NullEventSink};
