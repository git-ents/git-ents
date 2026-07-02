//! Interactive debug sessions into a repository's checks Sprite.
//!
//! A member who can already sign in to the web UI (see [`super::write`]) can
//! open a read-write shell in the same persistent Sprite a check run used,
//! brokered over a WebSocket so the member never needs a Fly credential of
//! their own: the server holds the one `SPRITES_TOKEN` and relays bytes.
//! Reachable at the reserved top-level path `/_debug/<repo-path>` — a repo
//! literally named `_debug` is shadowed, the same tradeoff `/login` already
//! makes against a repo named `login`.

use std::io::{Read as _, Write as _};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

use crate::AppState;

/// The pty's initial size, before the CLI's first resize control frame
/// arrives — the CLI sends one immediately on connecting, so this only
/// matters for the handful of frames in between.
const INITIAL_SIZE: PtySize = PtySize {
    rows: 24,
    cols: 80,
    pixel_width: 0,
    pixel_height: 0,
};

/// Upgrade an authenticated member's request into an interactive shell in
/// `repo_path`'s checks Sprite.
pub(crate) async fn handshake(
    State(state): State<AppState>,
    Path(repo_path): Path<String>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let segments: Vec<&str> = repo_path.split('/').filter(|s| !s.is_empty()).collect();
    let Some((repo, _rel, rest)) = super::resolve_repo(&state.data_dir, &segments) else {
        return (StatusCode::NOT_FOUND, "no such repository").into_response();
    };
    if !rest.is_empty() {
        return (StatusCode::NOT_FOUND, "no such repository").into_response();
    }

    let cookie = headers
        .get(axum::http::header::COOKIE)
        .and_then(|value| value.to_str().ok());
    let Some(session) = super::write::snapshot(&state.sessions, cookie) else {
        return (StatusCode::UNAUTHORIZED, "sign in to open a debug session").into_response();
    };
    let store = match git_store::Store::open(&repo) {
        Ok(store) => store,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("cannot open store: {e}"),
            )
                .into_response();
        }
    };
    if super::write::member_for_public_key_with(&store, &session.public_key).is_none() {
        return (
            StatusCode::FORBIDDEN,
            "your web key is not a member of this repository",
        )
            .into_response();
    }

    let sprite = crate::checks::sprite_name(&repo);
    let ready = tokio::task::spawn_blocking({
        let sprite = sprite.clone();
        move || crate::checks::ensure_auth().and_then(|()| crate::checks::ensure_sprite(&sprite))
    })
    .await;
    if !matches!(ready, Ok(Ok(()))) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not prepare the sprite",
        )
            .into_response();
    }

    ws.on_upgrade(move |socket| relay(socket, sprite))
}

/// Spawn an interactive shell in `sprite` and relay it over `socket` until
/// either side closes: the Sprite CLI's own `--tty` handles the remote
/// pseudo-TTY, but the broker allocates its *own* local pty for the `sprite
/// exec --tty` process so a resize control frame (see below) has something to
/// apply to — plain pipes have no window size to change.
async fn relay(mut socket: WebSocket, sprite: String) {
    let pair = match native_pty_system().openpty(INITIAL_SIZE) {
        Ok(pair) => pair,
        Err(_could_not_allocate) => return,
    };
    let mut cmd = CommandBuilder::new("sprite");
    cmd.args(["exec", "--tty", "-s", &sprite, "--", "/bin/bash"]);
    let mut child = match pair.slave.spawn_command(cmd) {
        Ok(child) => child,
        Err(_could_not_spawn) => return,
    };
    // Drop our copy of the slave side once the child holds it, so the
    // master's reader sees EOF when the child actually exits rather than
    // when this process happens to close it.
    drop(pair.slave);

    let master = pair.master;
    let (Ok(reader), Ok(mut writer)) = (master.try_clone_reader(), master.take_writer()) else {
        return;
    };

    // The pty's Read/Write are blocking, so each direction gets its own
    // thread; the read side hands chunks to the async loop over a channel,
    // the write side is fed the same way so a slow write never blocks the
    // select loop.
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut reader = reader;
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let Some(chunk) = buf.get(..n) else { break };
                    if out_tx.send(chunk.to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    let (in_tx, in_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        while let Ok(data) = in_rx.recv() {
            if writer.write_all(&data).is_err() {
                break;
            }
        }
    });

    loop {
        tokio::select! {
            chunk = out_rx.recv() => {
                match chunk {
                    Some(data) => {
                        if socket.send(Message::Binary(data.into())).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }
            frame = socket.recv() => {
                match frame {
                    Some(Ok(Message::Binary(data))) => {
                        if in_tx.send(data.to_vec()).is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Text(text))) => {
                        if let Some(size) = parse_resize(&text) {
                            let _resized = master.resize(size);
                        }
                    }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    _ => {}
                }
            }
        }
    }
    let _killed = child.kill();
}

/// Parse a resize control frame, `"<cols> <rows>"`, as sent by the CLI on
/// connect and on every local `SIGWINCH`.
fn parse_resize(text: &str) -> Option<PtySize> {
    let (cols, rows) = text.split_once(' ')?;
    Some(PtySize {
        cols: cols.parse().ok()?,
        rows: rows.parse().ok()?,
        pixel_width: 0,
        pixel_height: 0,
    })
}
