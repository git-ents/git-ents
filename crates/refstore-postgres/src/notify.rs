//! `LISTEN`/`NOTIFY` plumbing behind [`crate::PostgresRefStore::watch`].
//!
//! Postgres delivers notifications as out-of-band messages on the very
//! connection that issued `LISTEN`, surfaced through
//! [`tokio_postgres::Connection::poll_message`] rather than through
//! [`tokio_postgres::Client`]'s normal request/response methods. [`pump`]
//! drives that connection for the lifetime of the store, broadcasting every
//! payload to whichever [`crate::PostgresRefStore::watch`] calls are
//! currently subscribed; nothing here is trusted as a delivery guarantee —
//! see the trait-level contract on [`git_backend::RefStore::watch`].

use tokio::sync::broadcast;
use tokio_postgres::AsyncMessage;

/// The fixed channel every store `LISTEN`s on and `NOTIFY`s through. One
/// physical database can host many repositories' stores; each notification
/// payload is prefixed with its repo id so listeners on this shared channel
/// can filter out other repos' traffic (see [`decode`]).
pub(crate) const CHANNEL: &str = "git_ents_refstore";

/// How many undelivered payloads a slow [`git_backend::RefStore::watch`]
/// subscriber tolerates before it starts missing them. `watch` is a hint
/// only, so a lagging subscriber is told to assume something changed
/// (see [`crate::ref_store::bridge_watch`]) rather than silently losing
/// state.
pub(crate) const CHANNEL_CAPACITY: usize = 256;

/// Drive `connection`'s asynchronous messages for as long as the store
/// lives, forwarding every `NOTIFY` payload to `sender`. Errors and a closed
/// connection both end the pump silently: with nobody left to hand the
/// error to, the only correct move is to stop, which simply turns every
/// subsequent `watch` subscriber's hint stream quiet — never incorrect,
/// per the trait's best-effort contract.
pub(crate) async fn pump<S, T>(
    mut connection: tokio_postgres::Connection<S, T>,
    sender: broadcast::Sender<String>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        let message = std::future::poll_fn(|cx| connection.poll_message(cx)).await;
        match message {
            Some(Ok(AsyncMessage::Notification(notification))) => {
                let _ignored_no_subscribers = sender.send(notification.payload().to_owned());
            }
            Some(Ok(_)) => {}
            Some(Err(_)) | None => break,
        }
    }
}

/// Build the payload one [`crate::ref_store`] transaction `NOTIFY`s with:
/// the repo id, then one changed ref name per line. Ref names cannot
/// contain control characters, so `\n` is a safe separator with no
/// escaping needed.
pub(crate) fn encode(repo_id: &str, changed_names: &[String]) -> String {
    let mut payload = String::from(repo_id);
    for name in changed_names {
        payload.push('\n');
        payload.push_str(name);
    }
    payload
}

/// Whether `payload` (as built by [`encode`]) reports a change for
/// `repo_id` under `prefix`.
pub(crate) fn matches(payload: &str, repo_id: &str, prefix: &str) -> bool {
    let mut lines = payload.split('\n');
    if lines.next() != Some(repo_id) {
        return false;
    }
    lines.any(|name| name.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::{encode, matches};

    #[test]
    fn matches_filters_by_repo_and_prefix() {
        let payload = encode(
            "repo-a",
            &["refs/heads/main".to_owned(), "refs/meta/x".to_owned()],
        );
        assert!(matches(&payload, "repo-a", "refs/heads/"));
        assert!(matches(&payload, "repo-a", "refs/meta/"));
        assert!(!matches(&payload, "repo-a", "refs/cache/"));
        assert!(!matches(&payload, "repo-b", "refs/heads/"));
    }
}
