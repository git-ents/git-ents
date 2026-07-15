//! The pluggable ref store: reads plus atomic multi-ref compare-and-swap,
//! and a loose-ref implementation over gitoxide.
//!
//! This crate is the one place `git-ents` defines a trait gitoxide itself
//! is silent about (`arch.no-object-store-trait` names the ref store as
//! one of the seams that qualifies). It owns two things: the `RefStore`
//! trait, split into a read half ([`RefStoreRead`]) and a write half
//! ([`RefStore`]) per `arch.refstore-read-cas-split`, and
//! [`LooseRefStore`], the local default backend, which writes through
//! gitoxide's own in-process ref transaction rather than shelling out to
//! `git update-ref` (`arch.loose-cas-discipline`).
//!
//! The split exists for the gate (`gate.adoc`): verification is a pure
//! function over ref-store reads and must be statically incapable of
//! performing a write, so it is written against `RefStoreRead` alone.
//!
//! `LooseRefStore` delegates the mechanics of a write (the loose-file
//! format, reflog, packed-refs interaction) to gitoxide, but layers its
//! own serialization lock around every `transaction()` call — see the
//! `loose` module's doc comment for why: the pinned gitoxide version's
//! file-transaction precondition check reads a ref's value *before*
//! acquiring that ref's own lock, which is safe only when every writer
//! already funnels through one in-process handle. Two independent
//! `gix::Repository` handles racing the same ref (two `git-ents`
//! processes, most concretely) can otherwise both observe the same stale
//! precondition and both "win" a `MustNotExist`/`MustExistAndMatch` check.
//! `arch.loose-cas-discipline` asks for this store's *own* CAS discipline
//! for exactly this reason; `LooseRefStore` earns that literally rather
//! than trusting gitoxide's internal ordering to be enough on its own.
//!
//! # Spec coverage
//!
//! This crate implements, from `docs/spec/overview.sdoc`:
//!
//! - `arch.refstore-read-cas-split` — the `RefStoreRead`/`RefStore` split.
//! - `arch.loose-cas-discipline` — [`LooseRefStore`]'s use of gitoxide's
//!   own transaction machinery instead of a `git update-ref` subprocess.
//! - `arch.no-object-store-trait` — this crate defines exactly one new
//!   trait (the ref store), and touches object access only through
//!   gitoxide's own types.
//!
//! # Examples
//!
//! ```
//! use gix_hash::ObjectId;
//! use gix_ref_store::{Expected, LooseRefStore, RefEdit, RefStore, RefStoreRead, TxOutcome};
//!
//! # fn main() -> gix_ref_store::Result<()> {
//! let dir = tempfile::tempdir().expect("tempdir");
//! gix::init(dir.path()).expect("init");
//! let store = LooseRefStore::open(dir.path())?;
//!
//! let name: gix::refs::FullName = "refs/meta/config".try_into().expect("valid refname");
//! let oid = ObjectId::null(gix_hash::Kind::Sha1);
//!
//! // The read half alone is enough to observe the ref not existing yet —
//! // exactly what the gate is handed.
//! let read: &dyn RefStoreRead = &store;
//! assert_eq!(read.get(name.as_ref())?, None);
//!
//! // Only the write half can change it, and only via CAS.
//! let outcome = store.transaction(&[RefEdit {
//!     name: name.clone(),
//!     expected: Expected::MustNotExist,
//!     new: Some(oid),
//! }])?;
//! assert_eq!(outcome, TxOutcome::Applied);
//! assert_eq!(store.get(name.as_ref())?, Some(oid));
//! # Ok(())
//! # }
//! ```

mod edit;
mod error;
mod loose;
mod read;
mod store;

pub use edit::{Expected, RefEdit, RefIter, TxOutcome};
pub use error::{Error, Result};
pub use loose::LooseRefStore;
pub use read::RefStoreRead;
pub use store::RefStore;
