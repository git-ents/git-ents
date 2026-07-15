//! `git ents lsp`: reuse [`LocalRoot`]'s existing wiring and add only the
//! `ents-lens` Language Server Protocol frontend, over stdio (`lens.serve`).
//!
//! `lens.serve` requires this command to serve LSP over stdio reusing the
//! local composition root exactly as `git ents serve` reuses it for the web
//! UI (`roots.local`), binding no socket and adding no git transport. This
//! module upholds that: it is handed an already-open [`LocalRoot`] (never
//! opens its own), resolves the user's own signing key exactly as every
//! other mutation command does, and hands both to
//! [`ents_lens::serve_stdio`], which speaks only stdin/stdout.
//!
//! The signing identity is injected the same way `serve` injects
//! `ents-web`'s (`roots.web-agnostic` parity): the lens crate resolves no
//! key and assumes no editor is attached; this composition root builds an
//! owned [`ents_lens::Signing`] from the user's own key
//! (`roots.web-signing`'s local half — no server-key indirection exists
//! here) and moves it in.

use std::path::PathBuf;

use ents_lens::{Lens, Signing};

use super::{actor, signer};
use crate::error::{Error, Result};
use crate::root::LocalRoot;

/// Run `git ents lsp`: build the lens from `root`'s seams and the user's
/// resolved signing key, then serve LSP over stdio until the client shuts
/// it down.
///
/// Takes no output writer: the process's stdout is the LSP protocol
/// channel, so this command must write nothing else to it.
///
/// # Errors
///
/// Propagates a signing-key resolution failure ([`crate::sign::Signer`]),
/// or an [`Error::Io`] if the LSP transport fails.
// @relation(lens.serve, roots.local, scope=function)
pub fn run(root: LocalRoot, key: Option<PathBuf>) -> Result<()> {
    let signer = signer(&root, key)?;
    let identity_actor = actor(&signer);
    let public_openssh = signer.public_openssh();
    // The user's own key signs composed comments (`roots.web-signing`'s
    // local half): no server-key indirection is imported into the local
    // root, exactly as `serve` keeps it out.
    let signing = Signing::new(
        identity_actor,
        Box::new(move |payload| signer.sign(payload)),
        public_openssh,
    );

    let mode = root.mode();
    let LocalRoot {
        path,
        refs,
        objects,
        events,
        executor: _,
    } = root;
    let lens = Lens::new(
        Box::new(refs),
        objects,
        Box::new(events),
        mode,
        signing,
        path,
    );
    ents_lens::serve_stdio(lens).map_err(|source| Error::Io {
        path: PathBuf::from("<lsp stdio>"),
        source,
    })
}
