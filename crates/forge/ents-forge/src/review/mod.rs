//! The review sub-domain: the [`Review`] entity (`entity`), the `review`
//! command's business logic (`command`), and the `review` subcommand's
//! argument grammar (`cli`) — the same three-file split
//! [`crate::comment`] uses, for the same reason: the data shape, the
//! command mechanism, and the CLI grammar stay easy to read independently.

mod cli;
mod command;
mod entity;

pub use cli::ReviewAction;
pub use command::{NewReview, list, new, show};
pub use entity::Review;
