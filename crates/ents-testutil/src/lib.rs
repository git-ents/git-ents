//! Shared test fixtures for `git-ents` crates: an in-memory ref store, an
//! in-memory object store, deterministic signing keys, a signed-commit
//! builder, and seeding helpers for members, results, and code-ref history.
//!
//! This is the integration harness the engineering conventions call for —
//! a dev-dependency-only crate, never linked into a shipping binary. It
//! exists so `ents-gate` and `ents-query` (and later `ents-receive`,
//! `ents-sync`, `ents-effect`) exercise the same fixture vocabulary instead
//! of each reinventing repos, keys, and commit plumbing.
//!
//! Everything here panics on setup failure (via `expect`) rather than
//! returning `Result`: a fixture that cannot be built is a broken test,
//! not a condition under test.
//!
//! # Examples
//!
//! A complete miniature forge: one member enrolled under
//! `refs/meta/member/*`, one signed mutation, both readable back through
//! the same read-half trait production code uses.
//!
//! ```
//! use ents_model::Provenance;
//! use ents_testutil::{Keypair, MemRefStore, ObjectStore, enroll_member};
//! use gix_ref_store::RefStoreRead;
//!
//! let refs = MemRefStore::default();
//! let objects = ObjectStore::default();
//! let key = Keypair::from_seed(1);
//!
//! let tip = enroll_member(&refs, &objects, "jdc", &key, Provenance::AdminRegistered, 1_000);
//!
//! let name: gix::refs::FullName = "refs/meta/member/jdc".try_into().expect("valid");
//! assert_eq!(refs.get(name.as_ref()).expect("readable"), Some(tip));
//! ```

#![expect(
    clippy::expect_used,
    reason = "test-support crate: fixtures panic on setup failure rather than returning Result"
)]

pub use facet_git_tree::ObjectStore;

mod commit;
mod counting;
mod keys;
mod refs;
mod seed;

pub use commit::{CommitSpec, write_commit};
pub use counting::CountingFind;
pub use keys::Keypair;
pub use refs::MemRefStore;
pub use seed::{
    advance_ref, empty_tree, enroll_member, record_result, write_member, write_meta_entity,
};

/// Read a ref from a fixture store, panicking on the (impossible for
/// [`MemRefStore`]) backend error.
pub(crate) fn refs_get(
    refs: &MemRefStore,
    name: &gix::refs::FullName,
) -> Option<gix_hash::ObjectId> {
    use gix_ref_store::RefStoreRead as _;
    refs.get(name.as_ref())
        .expect("in-memory ref read cannot fail")
}
