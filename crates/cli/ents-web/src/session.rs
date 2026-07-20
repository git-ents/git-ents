//! Hosted web sessions (`roots.web-session`): held only in this server's
//! own process memory, never a session database or token table -- the
//! same ban `model.account` states for authentication state generally.
//!
//! [`SessionStore`] is a plain `Mutex<HashMap<..>>`; there is no on-disk or
//! external-database code path anywhere in this module for a session to
//! reach, so "memory only" is a structural property of the type, not a
//! configuration choice. A restarted process starts a new, empty
//! [`SessionStore`], which is exactly why every state-changing request
//! must additionally carry a per-session CSRF token: a stale cookie from a
//! previous process names a session this one has never heard of, and is
//! rejected as [`crate::Error::NoSession`] rather than silently trusted.

use std::collections::HashMap;
use std::sync::Mutex;

/// The cookie name a browser carries a session id in.
pub const COOKIE_NAME: &str = "ents_session";

/// The form field (or header, for a JSON-style client) a state-changing
/// request carries its CSRF token in.
pub const CSRF_FIELD: &str = "csrf";

/// The member a session proved control of a key for
/// (`roots.web-signin`): nothing but the username and the public key —
/// no secret is ever transmitted or stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMember {
    /// The enrolled member's username, as resolved at sign-in.
    pub username: String,
    /// The member's OpenSSH public key line, re-checked against the live
    /// member list at every mutation (`roots.web-signin`).
    pub key: String,
}

/// The current request's session id, inserted alongside [`Session`] by
/// the session middleware — a login page needs it to bind a challenge to
/// exactly this browser (`roots.web-signin`), and nothing else does: the
/// id is the cookie's secret, so handlers receive it in this deliberate
/// newtype rather than a bare `String` any format call could leak.
#[derive(Debug, Clone)]
pub struct SessionId(pub String);

/// One held session: the CSRF token it was issued, plus — once the
/// browser completes a sign-in (`roots.web-signin`) — the member it
/// authenticated as. Under a `Trusted` access policy
/// ([`crate::state::AccessPolicy`], the local root) `member` stays
/// `None` for every session and grants nothing either way: the injected
/// identity is the whole story there, so a session's only job is letting
/// this server recognize "the same browser that fetched the form is the
/// one submitting it," which the CSRF token proves.
// @relation(roots.web-session, roots.web-signin, scope=file)
#[derive(Debug, Clone)]
pub struct Session {
    /// The token a state-changing request must echo back.
    pub csrf: String,
    /// The member this session signed in as, if any.
    pub member: Option<SessionMember>,
}

/// Server-memory-only session storage (`roots.web-session`).
///
/// # Examples
///
/// ```
/// use ents_web::session::SessionStore;
///
/// let store = SessionStore::default();
/// let (id, session) = store.create();
/// assert_eq!(store.get(&id).expect("just created").csrf, session.csrf);
/// assert!(store.get("no-such-id").is_none());
/// ```
// @relation(roots.web-session, scope=file)
#[derive(Default)]
pub struct SessionStore {
    sessions: Mutex<HashMap<String, Session>>,
}

impl SessionStore {
    /// Mint a new session with a fresh random id and CSRF token, and hold
    /// it in memory.
    ///
    /// # Panics
    ///
    /// Never in practice: [`getrandom::fill`] only fails if the platform's
    /// randomness source itself is unavailable, which every supported
    /// target has.
    #[must_use]
    pub fn create(&self) -> (String, Session) {
        let id = random_token();
        let session = Session {
            csrf: random_token(),
            member: None,
        };
        #[expect(
            clippy::unwrap_used,
            reason = "a poisoned mutex means an earlier panic already unwound this process; \
                      there is no meaningful recovery for a session store, only a fresh restart"
        )]
        self.sessions
            .lock()
            .unwrap()
            .insert(id.clone(), session.clone());
        (id, session)
    }

    /// Look up a held session by id.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<Session> {
        #[expect(clippy::unwrap_used, reason = "see Self::create's identical reasoning")]
        self.sessions.lock().unwrap().get(id).cloned()
    }

    /// Mark `id`'s session as signed in as `member`
    /// (`roots.web-signin`), returning whether the session existed — a
    /// consumed challenge naming a session this store has never held (a
    /// restart between page load and sign-in) authenticates nothing.
    pub fn authenticate(&self, id: &str, member: SessionMember) -> bool {
        #[expect(clippy::unwrap_used, reason = "see Self::create's identical reasoning")]
        let mut sessions = self.sessions.lock().unwrap();
        match sessions.get_mut(id) {
            Some(session) => {
                session.member = Some(member);
                true
            }
            None => false,
        }
    }

    /// Drop `id`'s signed-in member, keeping the session itself — the
    /// logout action, and the auth middleware's response to a member
    /// revoked mid-session (`roots.web-signin`).
    pub fn clear_member(&self, id: &str) {
        #[expect(clippy::unwrap_used, reason = "see Self::create's identical reasoning")]
        let mut sessions = self.sessions.lock().unwrap();
        if let Some(session) = sessions.get_mut(id) {
            session.member = None;
        }
    }
}

/// A random, URL-safe token: 32 hex characters from 16 random bytes.
fn random_token() -> String {
    let mut bytes = [0u8; 16];
    #[expect(
        clippy::expect_used,
        reason = "getrandom only fails when the platform has no randomness source at all, which \
                  every target this crate ships to provides"
    )]
    getrandom::fill(&mut bytes).expect("platform randomness source is available");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Parse `Cookie:` header bytes for [`COOKIE_NAME`]'s value.
#[must_use]
pub fn session_id_from_cookie_header(header: &str) -> Option<&str> {
    header.split(';').find_map(|pair| {
        let (name, value) = pair.trim().split_once('=')?;
        (name == COOKIE_NAME).then_some(value)
    })
}

/// Render a `Set-Cookie` header value for `id` -- `HttpOnly` and
/// `SameSite=Strict` since this cookie is never read by page script and
/// only ever needs to accompany same-site requests (`roots.web-session`'s
/// CSRF requirement is the belt to this cookie's suspenders, not a
/// replacement for it: `SameSite=Strict` alone would already block a
/// cross-site POST, but a network intermediary or a future relaxation of
/// that attribute must not silently remove the protection).
///
/// `secure` adds the `Secure` attribute — set by the hosted deployment,
/// which only ever serves behind HTTPS, and never by local plain-HTTP
/// loopback serving, where the attribute would make the browser drop the
/// cookie entirely. Policy-driven, not deployment-sniffed: the caller
/// passes what its [`crate::state::AccessPolicy`] implies.
#[must_use]
pub fn set_cookie_header(id: &str, secure: bool) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!("{COOKIE_NAME}={id}; Path=/; HttpOnly; SameSite=Strict{secure}")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "unit test")]

    use rstest::rstest;

    use super::*;

    #[rstest]
    // @relation(roots.web-session, scope=function, role=Verifies)
    fn a_fresh_store_never_recognizes_a_foreign_session_id() {
        let a = SessionStore::default();
        let b = SessionStore::default();
        let (id, _) = a.create();
        assert!(
            b.get(&id).is_none(),
            "a session minted by one store must not be recognized by another -- there is no \
             shared backing store for either to consult"
        );
    }

    #[rstest]
    // @relation(roots.web-session, scope=function, role=Verifies)
    fn cookie_header_round_trips_the_session_id() {
        let header = set_cookie_header("abc123", false);
        assert!(header.contains("HttpOnly"));
        let raw_cookie = header.split(';').next().expect("at least one segment");
        assert_eq!(session_id_from_cookie_header(raw_cookie), Some("abc123"));
    }

    #[rstest]
    // @relation(roots.web-session, scope=function, role=Verifies)
    fn two_sessions_never_share_a_csrf_token() {
        let store = SessionStore::default();
        let (_, first) = store.create();
        let (_, second) = store.create();
        assert_ne!(first.csrf, second.csrf);
    }
}
