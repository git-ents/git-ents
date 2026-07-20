//! Hosted web sign-in (`roots.web-signin`): prove control of an enrolled,
//! active member key by signing a server-issued one-time challenge.
//!
//! The protocol is the pre-redo forge's key-proof sign-in with the paste
//! step replaced by `git ents login`: the `/login` page mints a short code
//! bound to the browser's own session, the CLI fetches the full challenge,
//! signs it under [`LOGIN_NAMESPACE`] — deliberately distinct from git's
//! `git` commit namespace, so a sign-in signature can never double as a
//! push signature or vice versa — and posts the signature back. The
//! server verifies it in pure Rust against the member's stored OpenSSH
//! public key, the same `ssh_key` technique `ents_gate::signature` uses
//! for commit signatures.
//!
//! Replay is bound off at every joint: the signed payload names the
//! serving host, the code, and the nonce ([`challenge_payload`]), so a
//! signature minted for one deployment or one browser session verifies
//! nowhere else; a challenge is single-use ([`ChallengeStore::take`]) and
//! expires after [`CHALLENGE_TTL`], measured on the monotonic clock.
// @relation(roots.web-signin, scope=file)

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ents_model::MemberState;
use gix_object::Find;
use ssh_key::{PublicKey, SshSig};

use crate::state::AppState;

/// The SSHSIG namespace a sign-in signature is made under — never `git`,
/// so a login signature and a push signature are unexchangeable
/// (`roots.web-signin`).
pub const LOGIN_NAMESPACE: &str = "git-ents-login";

/// How long an issued sign-in challenge stays valid. Measured with
/// [`Instant`], so a wall-clock step never extends or shortens a
/// challenge's life (a suspended machine's paused monotonic clock can
/// honor one slightly longer than this in wall time — accepted).
pub const CHALLENGE_TTL: Duration = Duration::from_secs(600);

/// The exact bytes both sides sign and verify: version-tagged, binding
/// the serving host, the browser code, and the one-time nonce. The CLI
/// MUST rebuild this locally from the host the member addressed rather
/// than signing server-supplied bytes (`roots.web-signin`).
#[must_use]
pub fn challenge_payload(host: &str, code: &str, nonce: &str) -> String {
    format!("git-ents-login-v1\nhost={host}\ncode={code}\nnonce={nonce}\n")
}

/// Normalize a user-facing code: the `/login` page displays `XXXX-XXXX`
/// and a human may retype it in either case, with or without the dash.
#[must_use]
pub fn normalize_code(code: &str) -> String {
    code.chars()
        .filter(|c| *c != '-')
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

/// One outstanding sign-in challenge: which browser session it will
/// authenticate, the nonce the signature must cover, and when it was
/// issued.
#[derive(Debug, Clone)]
pub struct Challenge {
    /// The session id the `/login` page bound this challenge to — the
    /// only session [`ChallengeStore::take`]'s caller may authenticate.
    pub session_id: String,
    /// The one-time nonce bound into [`challenge_payload`].
    pub nonce: String,
    /// When this challenge was issued, for [`CHALLENGE_TTL`].
    issued: Instant,
}

/// Outstanding sign-in challenges, keyed by their user-facing code —
/// memory-only, like [`crate::session::SessionStore`], and pruned of
/// expired entries on every issue.
#[derive(Default)]
pub struct ChallengeStore {
    table: Mutex<HashMap<String, Challenge>>,
}

impl ChallengeStore {
    /// Mint a fresh challenge bound to `session_id`, returning its
    /// `(code, nonce)`. Re-issuing for the same session replaces that
    /// session's outstanding challenge, so one browser holds at most one
    /// live code.
    #[must_use]
    pub fn issue(&self, session_id: &str) -> (String, String) {
        let code = random_code();
        let nonce = random_nonce();
        let challenge = Challenge {
            session_id: session_id.to_owned(),
            nonce: nonce.clone(),
            issued: Instant::now(),
        };
        let mut table = self.lock();
        let now = Instant::now();
        table.retain(|_code, held| {
            now.duration_since(held.issued) < CHALLENGE_TTL && held.session_id != session_id
        });
        table.insert(code.clone(), challenge);
        (code, nonce)
    }

    /// Read `code`'s live challenge without consuming it — the CLI's
    /// initial fetch. Expired entries read as absent.
    #[must_use]
    pub fn peek(&self, code: &str) -> Option<Challenge> {
        let code = normalize_code(code);
        let table = self.lock();
        let held = table.get(&code)?;
        (Instant::now().duration_since(held.issued) < CHALLENGE_TTL).then(|| held.clone())
    }

    /// Consume `code`, returning its challenge iff it was live and
    /// unexpired — single-use by construction: a second take of the same
    /// code is `None` whatever the first returned.
    #[must_use]
    pub fn take(&self, code: &str) -> Option<Challenge> {
        let code = normalize_code(code);
        let held = self.lock().remove(&code)?;
        (Instant::now().duration_since(held.issued) < CHALLENGE_TTL).then_some(held)
    }

    /// Age `code`'s challenge by `by`, as if it had been issued that much
    /// earlier — tests cannot construct an [`Instant`] in the past any
    /// other way.
    #[cfg(test)]
    fn backdate(&self, code: &str, by: Duration) {
        if let Some(held) = self.lock().get_mut(&normalize_code(code))
            && let Some(issued) = held.issued.checked_sub(by)
        {
            held.issued = issued;
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Challenge>> {
        self.table
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Whether `signature` (an armored SSHSIG) over `payload` verifies
/// against `public_key` (an OpenSSH single-line key, as stored on an
/// [`ents_model::Member`]) under [`LOGIN_NAMESPACE`]. Any malformed key,
/// malformed signature, wrong namespace, or failed cryptographic check is
/// `false` — mirrors `ents_gate::signature`'s identical posture for
/// commit signatures.
#[must_use]
pub fn verify_login(public_key: &str, payload: &[u8], signature: &str) -> bool {
    let Ok(key) = PublicKey::from_openssh(public_key) else {
        return false;
    };
    let Ok(sig) = SshSig::from_pem(signature) else {
        return false;
    };
    key.verify(LOGIN_NAMESPACE, payload, &sig).is_ok()
}

/// Resolve `pubkey` to the enrolled, *active* member that stores it, if
/// any — the sign-in completion's membership check, and the auth
/// middleware's per-mutation re-check (`roots.web-signin`: a session
/// whose member is no longer enrolled and active is refused at the time
/// of the mutation, not only at sign-in).
///
/// # Errors
///
/// Propagates a ref-store read failure; an individual member ref this
/// build cannot decode is skipped, exactly as the members page skips it.
pub(crate) fn active_member_by_key<O: Find>(
    state: &AppState<O>,
    pubkey: &str,
) -> crate::Result<Option<String>> {
    for (username, member) in crate::pages::members::read_all(state)? {
        if let Ok(member) = member
            && member.key == pubkey
            && member.state == MemberState::Active
        {
            return Ok(Some(username));
        }
    }
    Ok(None)
}

/// A user-facing code: eight characters of Crockford-style base32 (no
/// `I`, `L`, `O`, `U`, no lowercase), ~40 bits — plenty for a single-use
/// secret that lives ten minutes, and short enough to retype.
fn random_code() -> String {
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let mut bytes = [0u8; 8];
    fill_random(&mut bytes);
    bytes
        .iter()
        .map(|b| {
            // The low five bits index exactly the 32-entry alphabet, so
            // the draw is uniform and the index cannot overrun.
            let index = usize::from(*b & 0x1f);
            #[expect(
                clippy::indexing_slicing,
                reason = "a five-bit index cannot overrun the 32-entry alphabet"
            )]
            char::from(ALPHABET[index])
        })
        .collect()
}

/// A nonce: 32 hex characters from 16 random bytes, the same shape as a
/// session id.
fn random_nonce() -> String {
    let mut bytes = [0u8; 16];
    fill_random(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn fill_random(bytes: &mut [u8]) {
    #[expect(
        clippy::expect_used,
        reason = "getrandom only fails when the platform has no randomness source at all, which \
                  every target this crate ships to provides"
    )]
    getrandom::fill(bytes).expect("platform randomness source is available");
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "unit test")]

    use rstest::rstest;
    use ssh_key::private::{Ed25519Keypair, KeypairData};
    use ssh_key::{HashAlg, LineEnding, PrivateKey};

    use super::*;

    fn keypair(seed: u8) -> PrivateKey {
        let pair = Ed25519Keypair::from_seed(&[seed; 32]);
        PrivateKey::new(KeypairData::from(pair), "test").expect("well-formed")
    }

    fn sign_in_namespace(key: &PrivateKey, namespace: &str, payload: &[u8]) -> String {
        key.sign(namespace, HashAlg::Sha512, payload)
            .expect("signing is infallible for a loaded key")
            .to_pem(LineEnding::LF)
            .expect("an SSHSIG always renders as PEM")
    }

    fn public_line(key: &PrivateKey) -> String {
        key.public_key().to_openssh().expect("renders")
    }

    #[rstest]
    // @relation(roots.web-signin, scope=function, role=Verifies)
    fn payload_is_the_exact_versioned_bytes_both_sides_build() {
        assert_eq!(
            challenge_payload("git.ents.cloud", "ABCD2345", "0f" /* nonce */),
            "git-ents-login-v1\nhost=git.ents.cloud\ncode=ABCD2345\nnonce=0f\n"
        );
    }

    #[rstest]
    // @relation(roots.web-signin, scope=function, role=Verifies)
    fn a_challenge_is_single_use() {
        let store = ChallengeStore::default();
        let (code, nonce) = store.issue("session-1");
        let taken = store.take(&code).expect("first take");
        assert_eq!(taken.session_id, "session-1");
        assert_eq!(taken.nonce, nonce);
        assert!(store.take(&code).is_none(), "second take must fail");
    }

    #[rstest]
    // @relation(roots.web-signin, scope=function, role=Verifies)
    fn peek_reads_without_consuming_and_codes_normalize() {
        let store = ChallengeStore::default();
        let (code, nonce) = store.issue("session-1");
        let (head, tail) = code.split_at(4);
        let dashed = format!("{head}-{}", tail.to_lowercase());
        assert_eq!(store.peek(&dashed).expect("live").nonce, nonce);
        assert!(store.take(&dashed).is_some(), "peek must not consume");
    }

    #[rstest]
    // @relation(roots.web-signin, scope=function, role=Verifies)
    fn an_expired_challenge_reads_as_absent() {
        let store = ChallengeStore::default();
        let (code, _nonce) = store.issue("session-1");
        store.backdate(&code, CHALLENGE_TTL.saturating_add(Duration::from_secs(1)));
        assert!(store.peek(&code).is_none());
        assert!(store.take(&code).is_none());
    }

    #[rstest]
    // @relation(roots.web-signin, scope=function, role=Verifies)
    fn reissuing_replaces_the_sessions_outstanding_challenge() {
        let store = ChallengeStore::default();
        let (first, _) = store.issue("session-1");
        let (second, _) = store.issue("session-1");
        assert!(store.take(&first).is_none(), "replaced by the re-issue");
        assert!(store.take(&second).is_some());
    }

    #[rstest]
    // @relation(roots.web-signin, scope=function, role=Verifies)
    fn verify_accepts_only_the_login_namespace_over_the_exact_payload() {
        let key = keypair(1);
        let payload = challenge_payload("git.ents.cloud", "ABCD2345", "0011");
        let good = sign_in_namespace(&key, LOGIN_NAMESPACE, payload.as_bytes());
        assert!(verify_login(&public_line(&key), payload.as_bytes(), &good));

        // A real signature under git's own commit namespace over the very
        // same bytes must not double as a login (`roots.web-signin`).
        let push_shaped = sign_in_namespace(&key, "git", payload.as_bytes());
        assert!(!verify_login(
            &public_line(&key),
            payload.as_bytes(),
            &push_shaped
        ));

        // Any drift in the signed payload — another host, another code,
        // another nonce — fails verification.
        let other = challenge_payload("evil.example", "ABCD2345", "0011");
        assert!(!verify_login(&public_line(&key), other.as_bytes(), &good));

        // Another member's key does not verify this member's signature.
        assert!(!verify_login(
            &public_line(&keypair(2)),
            payload.as_bytes(),
            &good
        ));

        // Garbage inputs are false, never a panic.
        assert!(!verify_login("not a key", payload.as_bytes(), &good));
        assert!(!verify_login(
            &public_line(&key),
            payload.as_bytes(),
            "not a signature"
        ));
    }
}
