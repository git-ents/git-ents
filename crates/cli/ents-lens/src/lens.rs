//! The lens core: a read-time view over `refs/meta/comments/*` projected
//! into whatever buffer the client has open, plus the compose flow that
//! writes new comments back through the shared library.
//!
//! Every response is derived per request from an anchor projection onto the
//! working tree — never cached across a comment-ref mutation (`lens.lenses`)
//! — and every listing, projection, and write is the same
//! `ents_forge::comment` call the CLI porcelain makes (`lens.parity`), so a
//! comment is one entity across the editor, the CLI, and the web.

use std::path::PathBuf;

use ents_forge::comment::{self, ListFilter, Listed, NewComment};
use ents_receive::{EventSink, Identity, Mode};
use gix_object::{CommitRef, Find, Write};
use gix_ref_store::RefStore;
use lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, CodeLens, Command, Diagnostic, Hover,
    HoverContents, Position, Range, Url,
};
use serde_json::{Value, json};

use crate::compose::{self, Target};
use crate::document::{self, Documents};
use crate::error::{Error, Result};
use crate::render;
use crate::signing::Signing;

/// What an `executeCommand` or a `didSave` produced, in protocol-neutral
/// terms the server layer turns into LSP messages: an optional command
/// result value, an optional file to open with `window/showDocument`, and
/// whether the open documents' diagnostics should be republished
/// (a comment-ref mutation invalidates every derived view, `lens.lenses`).
#[derive(Debug, Default)]
pub struct Outcome {
    /// The `workspace/executeCommand` result value (the thread markup, for
    /// View); `None` for a command whose effect is a side effect only.
    pub response: Option<Value>,
    /// A file the client should open (`window/showDocument`) — the compose
    /// template, for Compose and Reply (`lens.compose`).
    pub show_document: Option<PathBuf>,
    /// Whether every open document's diagnostics should be recomputed and
    /// republished, because a comment ref just changed.
    pub refresh: bool,
}

/// The editor surface over one repository's comments. Holds the four
/// composition-root seams it needs (the ref store, the object store, the
/// event sink, and the signing identity — all injected, `lens.serve`), the
/// gate mode, the working-tree path, and the client's open buffers; owns no
/// derived state.
///
/// Generic over only the object store `O`, exactly as
/// `ents_web::state::AppState` is and for the same reason: `refs` and
/// `events` are already trait objects everywhere in this codebase, while
/// every mutation primitive takes the object store as `&(impl Find +
/// Write)`.
pub struct Lens<O> {
    refs: Box<dyn RefStore>,
    objects: O,
    events: Box<dyn EventSink>,
    mode: Mode,
    signing: Signing,
    path: PathBuf,
    documents: Documents,
}

impl<O: Find + Write> Lens<O> {
    /// Wire a lens from already-resolved seams — the one constructor a
    /// composition root calls (`git ents lsp`); the lens never opens a
    /// store or resolves a key itself.
    pub fn new(
        refs: Box<dyn RefStore>,
        objects: O,
        events: Box<dyn EventSink>,
        mode: Mode,
        signing: Signing,
        path: PathBuf,
    ) -> Self {
        Self {
            refs,
            objects,
            events,
            mode,
            signing,
            path,
            documents: Documents::default(),
        }
    }

    /// Record a document the client opened, with its full text
    /// (`textDocument/didOpen`) — projection targets this buffer afterward
    /// (`lens.working-tree`).
    pub fn did_open(&mut self, uri: Url, text: String) {
        self.documents.set(uri, text);
    }

    /// Replace an open document's text on a full-sync change
    /// (`textDocument/didChange`), so ranges re-project against the unsaved
    /// edit (`lens.working-tree`).
    pub fn did_change(&mut self, uri: Url, text: String) {
        self.documents.set(uri, text);
    }

    /// Forget a closed document (`textDocument/didClose`); projection falls
    /// back to on-disk bytes.
    pub fn did_close(&mut self, uri: &Url) {
        self.documents.remove(uri);
    }

    /// The open comments (`model.comment-state`) anchored to `uri`'s
    /// document, each projected onto its live buffer when open, on-disk
    /// bytes otherwise — the one derivation every read response is built
    /// from, recomputed here on every call and never cached (`lens.lenses`,
    /// `lens.working-tree`, `lens.parity`).
    ///
    /// # Errors
    ///
    /// Propagates a ref-store, object, repository, or projection failure.
    // @relation(lens.lenses, lens.working-tree, lens.parity, scope=function)
    fn document_comments(&self, uri: &Url) -> Result<Vec<Listed>> {
        let Some(rel) = document::relative_path(&self.path, uri) else {
            return Ok(Vec::new());
        };
        let buffer = self.documents.text(uri).map(str::as_bytes);
        let filter = ListFilter {
            state: Some("open".to_owned()),
            context: None,
        };
        let (rows, unreadable) = comment::list_for_document(
            self.refs.as_ref(),
            &self.objects,
            &self.path,
            &rel,
            buffer,
            &filter,
        )?;
        // An unreadable comment ref names no document, so it has no lens,
        // diagnostic, or hover to appear in -- the web listing's
        // disclosure and `git ents comment list`'s trailing note are the
        // surfaces that report it; here it is dropped deliberately, not
        // silently (the library returns it either way, `lens.parity`).
        let _ = unreadable;
        Ok(rows)
    }

    /// The code lenses for `uri` (`lens.lenses`): three per open comment
    /// that projects onto the document — a summary lens plus Reply and
    /// Resolve — omitting a comment whose anchor no longer lands there.
    ///
    /// # Errors
    ///
    /// Propagates a ref-store, object, repository, or projection failure.
    // @relation(lens.lenses, scope=function)
    pub fn code_lenses(&self, uri: &Url) -> Result<Vec<CodeLens>> {
        let mut out = Vec::new();
        for row in self.document_comments(uri)? {
            if let Some((range, outdated)) = landed(&row) {
                out.extend(render::code_lenses(&row.id, &row.comment, range, outdated));
            }
        }
        Ok(out)
    }

    /// The hint-severity diagnostics for `uri` (`lens.diagnostics`): the
    /// same projected comments as the lenses, one hint each, for clients
    /// that do not render lenses. Never a warning or error.
    ///
    /// # Errors
    ///
    /// Propagates a ref-store, object, repository, or projection failure.
    // @relation(lens.diagnostics, scope=function)
    pub fn diagnostics(&self, uri: &Url) -> Result<Vec<Diagnostic>> {
        let mut out = Vec::new();
        for row in self.document_comments(uri)? {
            if let Some((range, outdated)) = landed(&row) {
                out.push(render::diagnostic(&row.id, &row.comment, range, outdated));
            }
        }
        Ok(out)
    }

    /// The hover for a position in `uri` (`lens.hover`): if it falls on a
    /// projected comment's range, the whole thread rendered as Markdown —
    /// bodies, states, and authorship read from each ref's commit chain.
    ///
    /// # Errors
    ///
    /// Propagates a ref-store, object, repository, or projection failure.
    // @relation(lens.hover, scope=function)
    pub fn hover(&self, uri: &Url, position: Position) -> Result<Option<Hover>> {
        for row in self.document_comments(uri)? {
            let Some((range, _outdated)) = landed(&row) else {
                continue;
            };
            if position_in(position, range) {
                let markup = self.thread_markup(&row.id)?;
                return Ok(Some(Hover {
                    contents: HoverContents::Markup(markup),
                    range: Some(range),
                }));
            }
        }
        Ok(None)
    }

    /// The code actions for a selection in `uri` (`lens.compose`): the
    /// "Leave an ents comment" action, whose command opens the compose
    /// template anchored to exactly the selected lines against the working
    /// tree. Empty when the URI is not a file in the working tree.
    ///
    /// # Errors
    ///
    /// Never fails today; returns [`Result`] for symmetry with the other
    /// request handlers.
    // @relation(lens.compose, scope=function)
    pub fn code_actions(&self, uri: &Url, range: Range) -> Result<Vec<CodeActionOrCommand>> {
        let Some(rel) = document::relative_path(&self.path, uri) else {
            return Ok(Vec::new());
        };
        let lines = selection_lines(range);
        let command = Command {
            title: "Leave an ents comment".to_owned(),
            command: render::CMD_COMPOSE.to_owned(),
            arguments: Some(vec![json!({ "path": rel, "lines": lines })]),
        };
        Ok(vec![CodeActionOrCommand::CodeAction(CodeAction {
            title: "Leave an ents comment".to_owned(),
            kind: Some(CodeActionKind::EMPTY),
            diagnostics: None,
            edit: None,
            command: Some(command),
            is_preferred: None,
            disabled: None,
            data: None,
        })])
    }

    /// Run a `workspace/executeCommand` the lens registered
    /// (`lens.lenses`, `lens.compose`): View returns the thread, Resolve
    /// records the state mutation through the shared library call, and
    /// Reply/Compose open the compose template.
    ///
    /// # Errors
    ///
    /// [`Error::BadArguments`] for a missing or malformed argument;
    /// otherwise propagates the underlying comment library or template
    /// failure.
    // @relation(lens.lenses, lens.compose, lens.parity, scope=function)
    pub fn execute_command(&self, command: &str, arguments: &[Value]) -> Result<Outcome> {
        match command {
            render::CMD_VIEW => {
                let id = arg_id(arguments)?;
                let markup = self.thread_markup(&id)?;
                Ok(Outcome {
                    response: Some(json!(markup.value)),
                    ..Outcome::default()
                })
            }
            render::CMD_RESOLVE => {
                let id = arg_id(arguments)?;
                let signer = &self.signing;
                let sign = |payload: &[u8]| signer.sign(payload);
                let identity = Identity {
                    actor: signer.actor(),
                    sign: &sign,
                };
                comment::resolve(
                    self.refs.as_ref(),
                    &self.objects,
                    self.events.as_ref(),
                    &id,
                    &identity,
                    self.mode,
                )?;
                Ok(Outcome {
                    refresh: true,
                    ..Outcome::default()
                })
            }
            render::CMD_REPLY => {
                let id = arg_id(arguments)?;
                let target = Target {
                    parent: Some(id),
                    ..Target::default()
                };
                let template = self.write_template(&target)?;
                Ok(Outcome {
                    show_document: Some(template),
                    ..Outcome::default()
                })
            }
            render::CMD_COMPOSE => {
                let target = compose_target(arguments)?;
                let template = self.write_template(&target)?;
                Ok(Outcome {
                    show_document: Some(template),
                    ..Outcome::default()
                })
            }
            other => Err(Error::BadArguments(format!("unknown command {other}"))),
        }
    }

    /// Handle a `textDocument/didSave`: if the saved file is the compose
    /// template, finalize the comment (`lens.compose`); otherwise recompute
    /// diagnostics, since the saved buffer now matches disk.
    ///
    /// # Errors
    ///
    /// Propagates a template read or comment-creation failure.
    // @relation(lens.compose, scope=function)
    pub fn did_save(&self, uri: &Url) -> Result<Outcome> {
        if self.is_template(uri) {
            return self.finalize_compose();
        }
        Ok(Outcome {
            refresh: true,
            ..Outcome::default()
        })
    }

    /// Every open document's URI — the server republishes diagnostics for
    /// these after a mutation ([`Outcome::refresh`]).
    #[must_use]
    pub fn open_documents(&self) -> Vec<Url> {
        self.documents.open_uris()
    }

    /// Diagnostics for `uri` even when the document is not open — the server
    /// uses this to clear or refresh a specific document.
    ///
    /// # Errors
    ///
    /// See [`Lens::diagnostics`].
    pub fn diagnostics_for(&self, uri: &Url) -> Result<Vec<Diagnostic>> {
        self.diagnostics(uri)
    }

    /// The compose template's absolute path, `<git-dir>/ENTS_COMMENT_EDITMSG`
    /// (`lens.compose`).
    fn template_path(&self) -> Result<PathBuf> {
        let repo = gix::open(&self.path)?;
        Ok(repo.git_dir().join("ENTS_COMMENT_EDITMSG"))
    }

    /// Whether `uri` names the compose template (a saved-template event).
    fn is_template(&self, uri: &Url) -> bool {
        let Ok(template) = self.template_path() else {
            return false;
        };
        let Ok(saved) = uri.to_file_path() else {
            return false;
        };
        let template = template.canonicalize().unwrap_or(template);
        let saved = saved.canonicalize().unwrap_or(saved);
        saved == template
    }

    /// Write the compose template for `target` and return its path
    /// (`lens.compose`).
    fn write_template(&self, target: &Target) -> Result<PathBuf> {
        let template = self.template_path()?;
        let text = compose::template_text(target);
        std::fs::write(&template, text).map_err(|source| Error::Template {
            path: template.clone(),
            source,
        })?;
        Ok(template)
    }

    /// Read the saved template and create the comment it describes through
    /// the shared library call (`lens.parity`), anchoring to the working
    /// tree (`lens.working-tree`); an empty body aborts (`lens.compose`).
    /// The template is always removed afterward so a stale one is never
    /// reused.
    fn finalize_compose(&self) -> Result<Outcome> {
        let template = self.template_path()?;
        let content = std::fs::read_to_string(&template).map_err(|source| Error::Template {
            path: template.clone(),
            source,
        })?;
        let composed = compose::parse(&content);
        // Best effort: a leftover template is harmless — the next compose
        // overwrites it — so a removal failure never aborts a comment that
        // was otherwise created successfully.
        if let Err(_error) = std::fs::remove_file(&template) {}
        if composed.is_abort() {
            return Ok(Outcome::default());
        }

        let signer = &self.signing;
        let sign = |payload: &[u8]| signer.sign(payload);
        let identity = Identity {
            actor: signer.actor(),
            sign: &sign,
        };
        if let Some(parent) = composed.target.parent {
            comment::reply(
                self.refs.as_ref(),
                &self.objects,
                self.events.as_ref(),
                &parent,
                composed.body,
                &identity,
                self.mode,
            )?;
        } else {
            let new = NewComment {
                body: composed.body,
                path: composed.target.path,
                lines: composed.target.lines,
                rev: "HEAD".to_owned(),
                worktree: true,
                context: None,
                parent: None,
            };
            comment::add(
                self.refs.as_ref(),
                &self.objects,
                self.events.as_ref(),
                &self.path,
                new,
                &identity,
                self.mode,
            )?;
        }
        Ok(Outcome {
            refresh: true,
            ..Outcome::default()
        })
    }

    /// The whole thread rooted at `root_id` as hover Markdown (`lens.hover`)
    /// — root plus replies (`thread_of`), each stamped with the author and
    /// time read from its ref's tip mutation commit (`meta-ref.identity-binding`).
    fn thread_markup(&self, root_id: &str) -> Result<lsp_types::MarkupContent> {
        let rows = comment::thread_of(self.refs.as_ref(), &self.objects, root_id)?;
        let mut with_authors = Vec::with_capacity(rows.len());
        for (id, comment) in rows {
            let (author, when) = self
                .authorship(&id)
                .unwrap_or_else(|| ("unknown".to_owned(), String::new()));
            with_authors.push((id, comment, author, when));
        }
        Ok(render::hover_markup(&with_authors))
    }

    /// The author display name and a short date for the comment at `id`,
    /// read from its ref's tip mutation commit (`model.comment`: authorship
    /// lives in the commit chain, never a stored field). `None` when the
    /// ref or its commit cannot be read.
    fn authorship(&self, id: &str) -> Option<(String, String)> {
        let ref_name = ents_model::namespace::comment_ref(id).ok()?;
        let tip = self.refs.get(ref_name.as_ref()).ok().flatten()?;
        let mut buf = Vec::new();
        let data = Find::try_find(&self.objects, &tip, &mut buf).ok()??;
        let commit = CommitRef::from_bytes(data.data, tip.kind()).ok()?;
        let author = commit.author().ok()?;
        let name = author.name.to_string();
        let when = author
            .time()
            .ok()
            .and_then(|time| time.format(gix::date::time::format::SHORT).ok())
            .unwrap_or_default();
        Some((name, when))
    }
}

/// The `(range, outdated)` a listed comment lands at, or `None` when it does
/// not project onto the document (deleted, or unanchored).
fn landed(row: &Listed) -> Option<(Range, bool)> {
    let anchor = row.anchor.as_ref()?;
    let projection = row.projection.as_ref()?;
    render::landed_range(projection, anchor)
}

/// Whether `position`'s line falls within `range` — hovering anywhere on an
/// anchored line reveals its thread.
fn position_in(position: Position, range: Range) -> bool {
    position.line >= range.start.line && position.line <= range.end.line
}

/// The 1-based inclusive `<start>:<end>` line span a selection covers,
/// collapsing a trailing full-line boundary (`end` at column 0 of the next
/// line) back onto the last selected line.
fn selection_lines(range: Range) -> String {
    let start = range.start.line.saturating_add(1);
    let end = if range.end.character == 0 && range.end.line > range.start.line {
        range.end.line
    } else {
        range.end.line.saturating_add(1)
    };
    format!("{start}:{end}")
}

/// Extract a single comment-id string argument.
fn arg_id(arguments: &[Value]) -> Result<String> {
    arguments
        .first()
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| Error::BadArguments("expected a comment id argument".to_owned()))
}

/// Extract a [`Target`] from the `ents.compose` command's `{path,
/// lines}` object argument.
fn compose_target(arguments: &[Value]) -> Result<Target> {
    let object = arguments
        .first()
        .ok_or_else(|| Error::BadArguments("compose needs a target".to_owned()))?;
    Ok(Target {
        path: object
            .get("path")
            .and_then(Value::as_str)
            .map(str::to_owned),
        lines: object
            .get("lines")
            .and_then(Value::as_str)
            .map(str::to_owned),
        parent: object
            .get("parent")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}
