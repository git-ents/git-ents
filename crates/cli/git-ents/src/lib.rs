//! `git-ents`: the local root, the CLI-complete milestone
//! (`docs/development-plan.adoc`, phase 6).
//!
//! This crate's one responsibility is composition and porcelain: it wires
//! the four seams every other crate defines a trait for — `RefStore`
//! ([`gix_ref_store`]), the object store (gitoxide's own `Find`/`Write`),
//! `EventSink` ([`ents_receive`]), and `Executor` ([`ents_effect`]) — into
//! two composition roots ([`root`]), and exposes a subcommand surface
//! above them. No business logic lives here that a library crate should
//! own instead: every command module is a thin caller of `ents-gate`,
//! `ents-receive`, `ents-effect`, `ents-anchor`, or `ents-sync`.
//!
//! # Spec coverage
//!
//! From `docs/spec/roots.adoc`:
//!
//! - `roots.composition`, `roots.local` — [`root::LocalRoot`]: the plain
//!   CLI's composition root (loose-ref `RefStore`, the local odb, a null
//!   `EventSink`, the advisory gate).
//! - `roots.config-isolation` — every trait implementation is selected in
//!   [`root`] alone; no command module branches on configuration.
//! - `roots.worktree-update` — [`commands::setup`] sets
//!   `receive.denyCurrentBranch=updateInstead` on the local repository, the
//!   integration-test-harness edge case that spec section names.
//!
//! The development plan's phase-6 row additionally doubles this crate as
//! the single-node hosted root: [`root::HostedRoot`] and [`hook`] wire
//! loose refs and a real odb behind git's own `receive-pack`, an in-memory
//! `EventSink` reconciled at boot (`receive.reconstructible`), and a
//! `SpriteExecutor`. See those modules' own docs for the design this
//! deployment shape requires — the git-serving-transport case is
//! deliberately not `roots.hosted` (`git-ents-server`, phase 8, which
//! replaces the store itself); it is this same crate's wiring, reused,
//! per the plan's own framing.
//!
//! # Examples
//!
//! An end-to-end local write: enroll an admin member, then use it to
//! enroll a second member, mirroring what `git ents members add` does.
//!
//! ```
//! use ents_model::{MemberId, Provenance};
//! use ents_receive::{Identity, Mode, propose_entity};
//! use git_ents::mutate::outcome_to_result;
//! use git_ents::root::LocalRoot;
//! use git_ents::sign::Signer;
//! use gix_ref_store::RefStoreRead;
//!
//! # let dir = tempfile::tempdir().expect("tempdir");
//! # gix::init(dir.path()).expect("init");
//! # let key_path = dir.path().join("id_ed25519");
//! # {
//! #     use ssh_key::private::{Ed25519Keypair, KeypairData};
//! #     let pair = Ed25519Keypair::from_seed(&[3; 32]);
//! #     let key = ssh_key::PrivateKey::new(KeypairData::from(pair), "t").expect("well-formed");
//! #     key.write_openssh_file(&key_path, ssh_key::LineEnding::LF).expect("write");
//! # }
//! let root = LocalRoot::open(dir.path()).expect("opens");
//! let signer = Signer::load(&key_path).expect("loads");
//! let actor = gix::actor::Signature {
//!     name: "jdc".into(), email: "jdc@ents.test".into(),
//!     time: gix::date::Time { seconds: 1_000, offset: 0 },
//! };
//! let identity = Identity { actor, author: None, sign: &|payload| signer.sign(payload) };
//!
//! let member = ents_model::Member::new("jdc", signer.public_openssh(), Provenance::AdminRegistered);
//! let name = ents_model::namespace::member_ref(&MemberId::new("jdc")).expect("valid");
//! let outcome = propose_entity(
//!     &root.refs, &root.objects, &root.events, name.clone(), &member,
//!     &identity, "Enroll jdc", root.mode(),
//! ).expect("evaluates");
//! outcome_to_result(outcome, None).expect("bootstrap admits the first member");
//! assert!(root.refs.get(name.as_ref()).expect("reads").is_some());
//! ```

pub mod agent_worker;
pub mod cli;
pub mod commands;
pub mod error;
pub mod exe;
pub mod hook;
pub mod mutate;
pub mod package;
pub mod plan_worker;
pub mod review_worker;
pub mod root;
pub mod sign;

pub use error::{Error, Result};
