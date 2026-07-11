//! The toolchain sub-domain: the [`Toolchain`] entity ([`entity`]), its
//! resolution/materialization machinery ([`resolve`]), the `toolchain`
//! command's business logic ([`command`]), and the `toolchain`
//! subcommand's argument grammar ([`cli`]), kept in separate files so
//! the data shape, the resolution algorithm, the command mechanism, and
//! the CLI grammar stay easy to read independently — the same split
//! `ents-forge`'s `comment` sub-domain uses.

mod cli;
mod command;
mod entity;
mod recipe;

pub use cli::ToolchainAction;
pub use command::{import, list, log, view};
pub use entity::Toolchain;
pub use recipe::{Component, Recipe, cache_key, materialize, resolve};
