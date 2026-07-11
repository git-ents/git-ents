//! The comment sub-domain: the [`Comment`] entity ([`entity`]), the
//! `comment` command's business logic ([`command`]), and the `comment`
//! subcommand's argument grammar ([`cli`]), kept in separate files so
//! the data shape, the command mechanism, and the CLI grammar stay easy
//! to read independently.

mod cli;
mod command;
mod entity;

pub use cli::CommentAction;
pub use command::{add, list, show};
pub use entity::Comment;
