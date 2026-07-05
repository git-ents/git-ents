//! Git Ents core — the shared domain types read and written through
//! `git_store`, common to the CLI porcelain and the server.

pub mod account;
pub mod checks;
pub mod config;
pub mod issues;
#[cfg(test)]
mod testutil;

/// The all-zero object id git uses for a created or deleted ref in a push
/// (`<old> <new> <ref>` lines): a zero `<old>` is a create, a zero `<new>` a
/// delete.
pub const ZERO_OID: &str = "0000000000000000000000000000000000000000";
