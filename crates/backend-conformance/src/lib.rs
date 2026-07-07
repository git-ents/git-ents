//! The backend conformance suite — the property tests that *are* the
//! governing invariant (`docs/scale-out.adoc`, "Governing invariant"). One
//! semantics, enforced by conformance: every [`git_backend::RefStore`] and
//! [`git_backend::ObjectStore`] backend must pass the same properties,
//! defined once here rather than per backend.
//!
//! # Plugging in a new backend
//!
//! Add `backend-conformance` as a dev-dependency and a small test file that
//! calls [`ref_store_properties`] and/or [`object_store_properties`] with a
//! closure that builds a fresh instance of the backend under test:
//!
//! ```ignore
//! #[test]
//! fn conforms_to_ref_store_properties() {
//!     backend_conformance::ref_store_properties(|| WithScratchRepo::new(FilesRefStore::open));
//! }
//! ```
//!
//! Every property is generic over the trait, not any one backend, so a type
//! that satisfies `RefStore`/`ObjectStore` gets the whole suite for free.
//! [`WithScratchRepo`] is a convenience for file-backed local backends that
//! need a throwaway git repository to open against; a cloud backend's own
//! instantiation builds its backend however it needs to and does not have
//! to use it.

mod collector;
mod corpus;
mod fixture_oids;
mod object_store;
mod ref_store;
mod scratch_repo;
mod support;

pub use collector::{Collector, NoopCollector};
pub use corpus::{reachable_object_set, replay_corpus};
pub use fixture_oids::FixtureOids;
pub use object_store::{
    causal_collection_safety, object_store_properties, quarantine_invisibility,
};
pub use ref_store::{
    multi_ref_all_or_nothing, multi_ref_cas_concurrent_conflict, prefix_iteration_consistency,
    ref_store_properties, reflog_records_transactions, watch_loss_tolerance,
};
pub use scratch_repo::WithScratchRepo;
pub use support::{commit_oids_into, distinct_oids};
