//! The lens's view of the client's open buffers, and the file-URI ↔
//! repository-path arithmetic that ties an LSP document to the anchor
//! paths `refs/meta/comments/*` records.
//!
//! The lens caches nothing derived — no projection, no lens, no diagnostic
//! survives a comment-ref mutation (`lens.lenses`) — but it must remember
//! the *buffer text* the client has sent, because the client owns the only
//! copy of a document's unsaved content: `textDocument/didChange` ships an
//! edit, never the file, and the disk still holds the old bytes. Holding
//! the latest buffer per open URI is what lets projection target the bytes
//! the user is actually looking at (`lens.working-tree`), and it is dropped
//! the moment the client closes the document.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use lsp_types::Url;

/// The client's open text documents, keyed by URI, each holding the latest
/// full text the client has sent (`textDocumentSync` is full-text, so every
/// change replaces the whole buffer).
#[derive(Debug, Default)]
pub struct Documents {
    open: HashMap<Url, String>,
}

impl Documents {
    /// Record (or replace) the full text of the document at `uri` — the
    /// `didOpen`/`didChange` handler's whole job.
    pub fn set(&mut self, uri: Url, text: String) {
        self.open.insert(uri, text);
    }

    /// Forget the document at `uri` — `didClose`; projection falls back to
    /// the on-disk bytes afterward.
    pub fn remove(&mut self, uri: &Url) {
        self.open.remove(uri);
    }

    /// The latest buffer text for `uri`, if the client has it open.
    #[must_use]
    pub fn text(&self, uri: &Url) -> Option<&str> {
        self.open.get(uri).map(String::as_str)
    }

    /// Every open document's URI — the server republishes diagnostics for
    /// these after a comment-ref mutation.
    #[must_use]
    pub fn open_uris(&self) -> Vec<Url> {
        self.open.keys().cloned().collect()
    }
}

/// The repository-relative, forward-slashed path a file URI names inside
/// the working tree at `workdir`, or `None` when the URI is not a file
/// under it — the key that matches an [`ents_anchor::Anchor`]'s own
/// recorded path.
///
/// Both sides are canonicalized when possible (the working tree always
/// exists; an open document usually does), so a symlinked temp directory
/// like macOS's `/var` → `/private/var` does not defeat the prefix match;
/// when the document has no on-disk form yet, the raw paths are compared.
#[must_use]
pub fn relative_path(workdir: &Path, uri: &Url) -> Option<String> {
    let file = uri.to_file_path().ok()?;
    let file_canon = canonical(&file);
    let workdir_canon = canonical(workdir);
    let rel = file_canon.strip_prefix(&workdir_canon).ok()?;
    Some(rel.to_string_lossy().replace('\\', "/"))
}

/// Resolve `path` through symlinks even when its leaf does not exist yet,
/// by canonicalizing the deepest ancestor that does and re-appending the
/// rest — so a not-yet-saved document under a symlinked temp directory
/// (macOS's `/var` → `/private/var`) still shares a prefix with the
/// canonicalized working tree.
fn canonical(path: &Path) -> PathBuf {
    if let Ok(resolved) = path.canonicalize() {
        return resolved;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => canonical(parent).join(name),
        _ => path.to_owned(),
    }
}

/// The absolute file URI for `path` — used to open the compose template
/// with `window/showDocument`.
#[must_use]
pub fn file_uri(path: &Path) -> Option<Url> {
    Url::from_file_path(path).ok()
}
