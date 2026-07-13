//! The issue sub-domain: the [`Issue`] entity ([`entity`]), the `issue`
//! command's business logic ([`command`]), and the `issue` subcommand's
//! argument grammar ([`cli`]) — the same three-file split
//! [`crate::comment`] and [`crate::review`] use, for the same reason: the
//! data shape, the command mechanism, and the CLI grammar stay easy to
//! read independently.

mod cli;
mod command;
mod entity;

pub use cli::IssueAction;
pub use command::{EditIssue, NewIssue, edit, list, new, show};
pub use entity::Issue;
