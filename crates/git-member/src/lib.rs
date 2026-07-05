//! The Signed push abstraction's data model: who is trusted to push
//! (`members`), who has been struck from that trust (`revocations`), and
//! which refs a trusted member's role permits (`policy`).
//!
//! Verifying a push certificate against this trust set is a separate concern
//! — see `git-signed-push`.

pub mod members;
pub mod policy;
pub mod revocations;
#[cfg(test)]
mod testutil;

pub use policy::{glob_match, ref_allowed};
