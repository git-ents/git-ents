//! Interactive debug sessions into a repository's checks Sprite.
//!
//! A member who can already sign in to the web UI (see [`super::write`]) can
//! open a read-write shell in the same persistent Sprite a check run used,
//! brokered over a WebSocket so the member never needs a Fly credential of
//! their own: the server holds the one `SPRITES_TOKEN` and relays bytes.
//! Reachable at the reserved top-level path `/_debug/<repo-path>` — a repo
//! literally named `_debug` is shadowed, the same tradeoff `/login` already
//! makes against a repo named `login`.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::process::Command;

use crate::AppState;

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
/// either side closes: the Sprite CLI's own `--tty` handles the pseudo-TTY, so
/// the broker only ever pumps bytes.
async fn relay(mut socket: WebSocket, sprite: String) {
    let child = Command::new("sprite")
        .args(["exec", "--tty", "-s", &sprite, "--", "/bin/bash"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(_could_not_spawn) => return,
    };
    let (Some(mut stdin), Some(mut stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return;
    };

    let mut buf = [0u8; 4096];
    loop {
        tokio::select! {
            read = stdout.read(&mut buf) => {
                match read {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let Some(chunk) = buf.get(..n) else { break };
                        if socket.send(Message::Binary(chunk.to_vec().into())).await.is_err() {
                            break;
                        }
                    }
                }
            }
            frame = socket.recv() => {
                match frame {
                    Some(Ok(Message::Binary(data))) => {
                        if stdin.write_all(&data).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    _ => {}
                }
            }
        }
    }
    let _killed = child.kill().await;
}
