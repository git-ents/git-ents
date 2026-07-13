//! The stdio Language Server Protocol adapter (`lens.serve`): a thin,
//! synchronous dispatch loop over `lsp-server` that forwards each request to
//! a [`Lens`] method and turns the [`Outcome`] back into LSP messages.
//!
//! All derivation lives in [`Lens`]; this module only frames JSON-RPC,
//! declares capabilities, and routes. It binds no socket and adds no git
//! transport (`lens.serve`) — the only IO is stdin/stdout.
//!
//! `lsp-server` (rust-analyzer's own scaffold) is synchronous, which suits
//! the lens exactly: every operation it performs — reading `refs/meta/*`,
//! diffing against the working tree, writing a signed commit — is blocking
//! git and filesystem work, so an async runtime would only wrap blocking
//! calls in `spawn_blocking` for no gain. The framing and pack of message
//! types come from the crate; nothing here hand-rolls JSON-RPC.
#![expect(
    clippy::let_underscore_must_use,
    reason = "sending on the LSP transport is best-effort: a closed pipe ends the loop on the \
              next receive, so a failed send needs no separate handling"
)]

use gix_object::{Find, Write};
use lsp_server::{Connection, ExtractError, Message, Request, RequestId, Response};
use lsp_types::notification::{
    DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, DidSaveTextDocument,
    Notification as _, PublishDiagnostics,
};
use lsp_types::request::{
    CodeActionRequest, CodeLensRequest, ExecuteCommand, HoverRequest, Request as _, ShowDocument,
};
use lsp_types::{
    CodeActionOptions, CodeActionProviderCapability, CodeLensOptions, DidChangeTextDocumentParams,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, DidSaveTextDocumentParams,
    ExecuteCommandOptions, HoverProviderCapability, PublishDiagnosticsParams, ServerCapabilities,
    ShowDocumentParams, TextDocumentSyncCapability, TextDocumentSyncKind, TextDocumentSyncOptions,
    TextDocumentSyncSaveOptions, Url,
};

use crate::lens::{Lens, Outcome};
use crate::render;

/// The capabilities the lens advertises (`lens.serve`): full-text document
/// sync with save notifications (the compose flow needs the save,
/// `lens.compose`), code lenses (`lens.lenses`), hover (`lens.hover`), code
/// actions (`lens.compose`), and the four executable commands
/// (`lens.lenses`, `lens.compose`). No workspace, symbol, or completion
/// surface — the lens is a conversation view, not a language analyzer.
#[must_use]
pub fn capabilities() -> ServerCapabilities {
    ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Options(
            TextDocumentSyncOptions {
                open_close: Some(true),
                change: Some(TextDocumentSyncKind::FULL),
                save: Some(TextDocumentSyncSaveOptions::Supported(true)),
                ..TextDocumentSyncOptions::default()
            },
        )),
        code_lens_provider: Some(CodeLensOptions {
            resolve_provider: Some(false),
        }),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        code_action_provider: Some(CodeActionProviderCapability::Options(CodeActionOptions {
            ..CodeActionOptions::default()
        })),
        execute_command_provider: Some(ExecuteCommandOptions {
            commands: vec![
                render::CMD_VIEW.to_owned(),
                render::CMD_REPLY.to_owned(),
                render::CMD_RESOLVE.to_owned(),
                render::CMD_COMPOSE.to_owned(),
            ],
            work_done_progress_options: lsp_types::WorkDoneProgressOptions::default(),
        }),
        ..ServerCapabilities::default()
    }
}

/// Serve the lens over stdio until the client shuts it down (`lens.serve`).
///
/// Performs the LSP initialize handshake advertising [`capabilities`], then
/// runs the dispatch loop. Binds no socket and speaks only stdin/stdout.
///
/// # Errors
///
/// [`std::io::Error`] if the JSON-RPC transport fails (a broken pipe, a
/// malformed frame) or the initialize handshake does not complete.
pub fn serve_stdio<O: Find + Write>(lens: Lens<O>) -> std::io::Result<()> {
    let (connection, io_threads) = Connection::stdio();
    let capabilities = serde_json::to_value(capabilities())
        .map_err(|source| std::io::Error::other(source.to_string()))?;
    let _init_params = connection
        .initialize(capabilities)
        .map_err(|source| std::io::Error::other(source.to_string()))?;
    let mut server = ServerLoop { connection, lens };
    server.run()?;
    io_threads.join()?;
    Ok(())
}

/// The running dispatch loop: owns the connection and the lens, and a
/// counter for the ids of the server-initiated requests it sends (only
/// `window/showDocument`).
struct ServerLoop<O> {
    connection: Connection,
    lens: Lens<O>,
}

impl<O: Find + Write> ServerLoop<O> {
    fn run(&mut self) -> std::io::Result<()> {
        // `iter()` yields until the client closes the connection; a
        // `shutdown` request breaks the loop through `handle_shutdown`.
        while let Ok(message) = self.connection.receiver.recv() {
            match message {
                Message::Request(request) => {
                    if self
                        .connection
                        .handle_shutdown(&request)
                        .map_err(|source| std::io::Error::other(source.to_string()))?
                    {
                        break;
                    }
                    self.on_request(request);
                }
                Message::Notification(notification) => self.on_notification(notification),
                // Responses to our own `window/showDocument` requests carry
                // nothing the lens needs to act on.
                Message::Response(_) => {}
            }
        }
        Ok(())
    }

    /// Route one request to a [`Lens`] read handler, replying with its
    /// result or an error response.
    fn on_request(&mut self, request: Request) {
        let request = match self.request::<CodeLensRequest, _>(request, |lens, params| {
            lens.code_lenses(&params.text_document.uri).map(Some)
        }) {
            Ok(()) => return,
            Err(request) => request,
        };
        let request = match self.request::<HoverRequest, _>(request, |lens, params| {
            let position = params.text_document_position_params;
            lens.hover(&position.text_document.uri, position.position)
        }) {
            Ok(()) => return,
            Err(request) => request,
        };
        let request = match self.request::<CodeActionRequest, _>(request, |lens, params| {
            lens.code_actions(&params.text_document.uri, params.range)
                .map(Some)
        }) {
            Ok(()) => return,
            Err(request) => request,
        };
        // `executeCommand` is the one request with side effects, handled on
        // its own so its `Outcome` can drive `showDocument` and refreshes.
        let request = match self.on_execute_command(request) {
            Ok(()) => return,
            Err(request) => request,
        };
        // Any other request: an empty success, so a client probing an
        // unsupported method gets a well-formed (null) reply, never a hang.
        self.respond(Response::new_ok(request.id, serde_json::Value::Null));
    }

    /// Extract and answer a plain read request `R`, returning `Err(request)`
    /// unchanged when it is a different method so the caller can try the
    /// next.
    fn request<R, F>(&mut self, request: Request, handle: F) -> Result<(), Request>
    where
        R: lsp_types::request::Request,
        F: FnOnce(&Lens<O>, R::Params) -> crate::error::Result<R::Result>,
        R::Result: serde::Serialize,
    {
        match request.extract::<R::Params>(R::METHOD) {
            Ok((id, params)) => {
                let response = match handle(&self.lens, params) {
                    Ok(result) => Response::new_ok(id, result),
                    Err(error) => error_response(id, &error),
                };
                self.respond(response);
                Ok(())
            }
            Err(ExtractError::MethodMismatch(request)) => Err(request),
            Err(ExtractError::JsonError { .. }) => {
                // A malformed params payload for a method we do own: there is
                // no id to reply against cleanly here, so drop it — the
                // client will observe the missing response.
                Ok(())
            }
        }
    }

    /// Handle `workspace/executeCommand`, applying the [`Outcome`]: reply
    /// with its result value, open the compose template if it named one, and
    /// republish diagnostics when a mutation invalidated them.
    fn on_execute_command(&mut self, request: Request) -> Result<(), Request> {
        match request.extract::<<ExecuteCommand as lsp_types::request::Request>::Params>(
            ExecuteCommand::METHOD,
        ) {
            Ok((id, params)) => {
                match self
                    .lens
                    .execute_command(&params.command, &params.arguments)
                {
                    Ok(outcome) => {
                        let value = outcome.response.clone().unwrap_or(serde_json::Value::Null);
                        self.respond(Response::new_ok(id, value));
                        self.apply(outcome);
                    }
                    Err(error) => self.respond(error_response(id, &error)),
                }
                Ok(())
            }
            Err(ExtractError::MethodMismatch(request)) => Err(request),
            Err(ExtractError::JsonError { .. }) => Ok(()),
        }
    }

    /// Route one notification to the matching [`Lens`] document or save
    /// handler, republishing diagnostics as the sync events demand.
    fn on_notification(&mut self, notification: lsp_server::Notification) {
        match notification.method.as_str() {
            DidOpenTextDocument::METHOD => {
                if let Ok(params) = extract_notification::<DidOpenTextDocumentParams>(notification)
                {
                    let uri = params.text_document.uri.clone();
                    self.lens.did_open(uri.clone(), params.text_document.text);
                    self.publish(&uri);
                }
            }
            DidChangeTextDocument::METHOD => {
                if let Ok(params) =
                    extract_notification::<DidChangeTextDocumentParams>(notification)
                {
                    let uri = params.text_document.uri.clone();
                    // Full sync: the last change carries the whole buffer.
                    if let Some(change) = params.content_changes.into_iter().next_back() {
                        self.lens.did_change(uri.clone(), change.text);
                    }
                    self.publish(&uri);
                }
            }
            DidCloseTextDocument::METHOD => {
                if let Ok(params) = extract_notification::<DidCloseTextDocumentParams>(notification)
                {
                    self.lens.did_close(&params.text_document.uri);
                    // Clear this document's diagnostics on close.
                    self.publish_list(&params.text_document.uri, Vec::new());
                }
            }
            DidSaveTextDocument::METHOD => {
                if let Ok(params) = extract_notification::<DidSaveTextDocumentParams>(notification)
                {
                    match self.lens.did_save(&params.text_document.uri) {
                        Ok(outcome) => self.apply(outcome),
                        Err(error) => log(&format!("didSave: {error}")),
                    }
                }
            }
            _ => {}
        }
    }

    /// Apply an [`Outcome`]'s side effects: open the compose template and/or
    /// republish diagnostics for every open document.
    fn apply(&mut self, outcome: Outcome) {
        if let Some(path) = outcome.show_document
            && let Some(uri) = crate::document::file_uri(&path)
        {
            self.show_document(uri);
        }
        if outcome.refresh {
            for uri in self.lens.open_documents() {
                self.publish(&uri);
            }
        }
    }

    /// Compute and publish diagnostics for one document (`lens.diagnostics`).
    fn publish(&self, uri: &Url) {
        match self.lens.diagnostics_for(uri) {
            Ok(diagnostics) => self.publish_list(uri, diagnostics),
            Err(error) => log(&format!("diagnostics for {uri}: {error}")),
        }
    }

    /// Send a `textDocument/publishDiagnostics` notification.
    fn publish_list(&self, uri: &Url, diagnostics: Vec<lsp_types::Diagnostic>) {
        let params = PublishDiagnosticsParams {
            uri: uri.clone(),
            diagnostics,
            version: None,
        };
        let notification =
            lsp_server::Notification::new(PublishDiagnostics::METHOD.to_owned(), params);
        let _ = self
            .connection
            .sender
            .send(Message::Notification(notification));
    }

    /// Ask the client to open `uri` (`window/showDocument`) — the compose
    /// template (`lens.compose`).
    fn show_document(&self, uri: Url) {
        let params = ShowDocumentParams {
            uri,
            external: Some(false),
            take_focus: Some(true),
            selection: None,
        };
        let request = Request {
            // A fixed id: the lens never correlates showDocument responses,
            // and only one is ever in flight per user action.
            id: RequestId::from("ents-show-document".to_owned()),
            method: ShowDocument::METHOD.to_owned(),
            params: serde_json::to_value(params).unwrap_or(serde_json::Value::Null),
        };
        let _ = self.connection.sender.send(Message::Request(request));
    }

    fn respond(&self, response: Response) {
        let _ = self.connection.sender.send(Message::Response(response));
    }
}

/// Build an LSP error response from a lens error, so a failing request gets
/// a well-formed fault rather than a dropped reply.
fn error_response(id: RequestId, error: &crate::error::Error) -> Response {
    Response::new_err(
        id,
        lsp_server::ErrorCode::RequestFailed as i32,
        error.to_string(),
    )
}

/// Deserialize a notification's params, returning `Err` on a mismatch or a
/// malformed payload.
fn extract_notification<P: serde::de::DeserializeOwned>(
    notification: lsp_server::Notification,
) -> Result<P, ()> {
    serde_json::from_value(notification.params).map_err(|_error| ())
}

/// Emit a diagnostic line on stderr — the lens's own log channel, since
/// stdout carries the LSP framing.
fn log(message: &str) {
    eprintln!("ents-lens: {message}");
}
