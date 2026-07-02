//! The CLI side of an interactive debug session: connect to the server's
//! WebSocket broker (see `git-ents-server`'s `web::debug`), put this
//! terminal into raw mode, and pump bytes between it and the remote shell
//! until either side closes.

use std::io::{Read as _, Write as _};

use futures_util::{SinkExt as _, StreamExt as _};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

/// Open a debug session at `url`, authenticated with the session `token`
/// stored by `git ents login`.
pub(crate) async fn run(url: &str, token: &str) -> Result<(), String> {
    let mut request = url
        .into_client_request()
        .map_err(|error| format!("bad debug session URL: {error}"))?;
    let cookie = format!("ents_session={token}")
        .parse()
        .map_err(|_invalid| "the stored session token is not a valid cookie value".to_owned())?;
    request.headers_mut().insert("Cookie", cookie);

    let (stream, _response) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|error| format!("could not open the debug session: {error}"))?;
    let (mut sink, mut source) = stream.split();

    crossterm::terminal::enable_raw_mode()
        .map_err(|error| format!("could not set the terminal to raw mode: {error}"))?;
    let result = pump(&mut sink, &mut source).await;
    let _restored = crossterm::terminal::disable_raw_mode();
    result
}

/// Relay bytes both ways: a background thread feeds raw stdin bytes through
/// `tx`, forwarded here to the sink, while frames from `source` are written
/// straight to stdout. A dedicated thread reads stdin because raw terminal
/// input has no natural way to interrupt a blocking read when the session
/// ends from the other side.
async fn pump<S, R>(sink: &mut S, source: &mut R) -> Result<(), String>
where
    S: futures_util::Sink<Message> + Unpin,
    R: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 1024];
        loop {
            match std::io::stdin().read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let Some(chunk) = buf.get(..n) else { break };
                    if tx.send(chunk.to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let (resize_tx, mut resize_rx) = tokio::sync::mpsc::unbounded_channel::<(u16, u16)>();
    spawn_resize_watcher(resize_tx.clone());
    // Size the remote pty to match this terminal before the first byte flows,
    // rather than leaving it at the broker's default until the first resize.
    if let Ok(size) = crossterm::terminal::size() {
        let _sent = resize_tx.send(size);
    }

    loop {
        tokio::select! {
            input = rx.recv() => {
                match input {
                    Some(bytes) => {
                        if sink.send(Message::Binary(bytes.into())).await.is_err() {
                            return Ok(());
                        }
                    }
                    None => return Ok(()),
                }
            }
            resize = resize_rx.recv() => {
                if let Some((cols, rows)) = resize {
                    let frame = Message::Text(format!("{cols} {rows}").into());
                    if sink.send(frame).await.is_err() {
                        return Ok(());
                    }
                }
            }
            frame = source.next() => {
                match frame {
                    Some(Ok(Message::Binary(data))) => {
                        let _write = std::io::stdout().write_all(&data);
                        let _flush = std::io::stdout().flush();
                    }
                    Some(Ok(Message::Close(_))) | None => return Ok(()),
                    Some(Err(error)) => return Err(format!("debug session error: {error}")),
                    _ => {}
                }
            }
        }
    }
}

/// Watch for local terminal resizes (`SIGWINCH`) on a dedicated thread,
/// sending the new `(cols, rows)` through `tx` each time. `crossterm`'s own
/// event reader also reports resizes, but isn't an option here — we forward
/// raw stdin bytes rather than events it would parse and consume them from.
///
/// `SIGWINCH` doesn't exist outside Unix, so there's no resize forwarding
/// on other platforms.
#[cfg(unix)]
fn spawn_resize_watcher(tx: tokio::sync::mpsc::UnboundedSender<(u16, u16)>) {
    let Ok(mut signals) = signal_hook::iterator::Signals::new([signal_hook::consts::SIGWINCH])
    else {
        return;
    };
    std::thread::spawn(move || {
        for _signal in signals.forever() {
            let Ok(size) = crossterm::terminal::size() else {
                continue;
            };
            if tx.send(size).is_err() {
                break;
            }
        }
    });
}

#[cfg(not(unix))]
fn spawn_resize_watcher(_tx: tokio::sync::mpsc::UnboundedSender<(u16, u16)>) {}
