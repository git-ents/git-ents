//! The lens: an editor-facing Language Server Protocol surface over
//! `refs/meta/comments/*`, projecting the repository's anchored comments
//! into whatever buffer the user is reading and writing new ones back
//! through the same signed mutation path every other frontend uses.
//!
//! # One responsibility
//!
//! This crate is the third place a git-ents conversation surfaces, after
//! the CLI and the web UI, and it earns no third mechanism (`docs/spec/lens.adoc`):
//! it is a read-time *view* over `refs/meta/*`, owning no state of its own,
//! so a comment left in an editor, on the web, or by an agent at the CLI is
//! one and the same entity everywhere. Every listing, projection, and write
//! is the exact `ents_forge::comment` library call the `git ents comment`
//! porcelain makes (`lens.parity`); the lens never shells out and never
//! reimplements listing or projection. It is a frontend of the local root
//! and receives its signing identity by injection (`lens.serve`,
//! `roots.web-agnostic`), exactly as `ents-web` does.
//!
//! # Spec coverage (`docs/spec/lens.adoc`)
//!
//! - `lens.serve` — [`serve_stdio`], stdio only, no socket, no git
//!   transport; the signing identity is the injected [`Signing`].
//! - `lens.lenses` — [`Lens::code_lenses`]: one View/Reply/Resolve lens set
//!   per open comment projecting onto the document, derived per request.
//! - `lens.diagnostics` — [`Lens::diagnostics`]: the same comments as
//!   hint-severity diagnostics, never warnings or errors.
//! - `lens.hover` — [`Lens::hover`]: the full thread as Markdown, authorship
//!   read from each ref's commit chain.
//! - `lens.compose` — [`Lens::code_actions`] plus the compose flow in
//!   [`compose`]: a code action opens a git-style template file, saving a
//!   non-empty body creates the comment.
//! - `lens.working-tree` — projection targets the working tree, the open
//!   buffer standing in for disk, re-projected on every change.
//! - `lens.parity` — every operation is an `ents_forge::comment` call.
//!
//! # The compose-on-save mechanism
//!
//! Composing works entirely through standard LSP a plain client provides —
//! `workspace/executeCommand`, `window/showDocument`, and
//! `textDocument/didSave` — with no client-specific extension. The
//! `ents.compose` command writes a git-commit-style template under
//! `.git/ENTS_COMMENT_EDITMSG` and asks the client to open it; when the user
//! saves it, the `didSave` handler creates the comment (or aborts on an
//! empty body). See [`compose`] for the exact grammar and rationale.
//!
//! # Worked example
//!
//! Wire a lens against a fresh repository (as `git ents lsp`'s composition
//! root does) and ask it for the code lenses on a document — none yet, since
//! no comment has been written:
//!
//! ```
//! use ents_lens::{Lens, Signing};
//! use ents_receive::{Mode, NullEventSink};
//! use ents_testutil::{MemRefStore, ObjectStore};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let dir = tempfile::tempdir()?;
//! gix::init(dir.path())?;
//!
//! // The composition root injects the signing identity (`lens.serve`); a
//! // fixed fixture stands in for the user's own resolved key here.
//! let signing = Signing::new(
//!     gix::actor::Signature {
//!         name: "jdc".into(),
//!         email: "jdc@ents.test".into(),
//!         time: gix::date::Time { seconds: 0, offset: 0 },
//!     },
//!     Box::new(|_payload| "-----BEGIN SSH SIGNATURE-----\n-----END SSH SIGNATURE-----\n".to_owned()),
//!     "ssh-ed25519 AAAA jdc".to_owned(),
//! );
//!
//! let lens = Lens::new(
//!     Box::new(MemRefStore::default()),
//!     ObjectStore::default(),
//!     Box::new(NullEventSink),
//!     Mode::Advisory,
//!     signing,
//!     dir.path().to_owned(),
//! );
//!
//! let uri = lsp_types::Url::from_file_path(dir.path().join("src/lib.rs")).unwrap();
//! assert!(lens.code_lenses(&uri)?.is_empty());
//! assert!(lens.diagnostics(&uri)?.is_empty());
//! # Ok(())
//! # }
//! ```

pub mod compose;
mod document;
mod error;
mod lens;
mod render;
mod server;
mod signing;

pub use compose::{Composed, Target};
pub use error::{Error, Result};
pub use lens::{Lens, Outcome};
pub use render::{CMD_COMPOSE, CMD_REPLY, CMD_RESOLVE, CMD_VIEW};
pub use server::{capabilities, serve_stdio};
pub use signing::Signing;
