//! Git Ents — helpful guardians of your git trees.

pub mod account;
pub mod checks;
pub mod comments;
pub mod config;
pub mod issues;
pub mod members;
pub mod reviews;
pub mod revocations;
#[cfg(test)]
mod testutil;

/// The all-zero object id git uses for a created or deleted ref in a push
/// (`<old> <new> <ref>` lines): a zero `<old>` is a create, a zero `<new>` a
/// delete.
pub const ZERO_OID: &str = "0000000000000000000000000000000000000000";
