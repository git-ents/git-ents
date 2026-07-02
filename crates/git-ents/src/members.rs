//! The repository's members, sourced from the `refs/meta/member/*` refs.
//!
//! A *member* is one person whose signed pushes the repository accepts. The
//! OpenSSH `allowed_signers` file this module renders keeps that name because it
//! is OpenSSH's own format term, but everything else here is framed as members.
//!
//! Push authentication trusts exactly one place: the `refs/meta/member/<username>`
//! refs. Each is a [`Member`] document — one person, named by the ref's last
//! segment — recording the keys whose signed pushes are accepted and the window
//! that trust holds within. The set is decomposed, one ref per person, rather
//! than a single aggregated blob, so a member can be added, refreshed, or revoked
//! as an independent, separately-history'd ref. The verifier unions every
//! `refs/meta/member/*` into the trust list.
//!
//! # Expiry
//!
//! Each member carries an optional `valid-after`/`valid-before` window rendered
//! into the `allowed_signers` file git verifies pushes against. The window is the
//! security primitive: an un-refreshed member stops authorizing *new* pushes once
//! it lapses, so stale trust fails closed. A previously-valid push stays
//! verifiable forever, since `ssh-keygen -Y verify -Overify-time` can pin the
//! check to the time the push was made.
//!
//! # Provenance and `Trust::WebAuthn`
//!
//! A member is ordinarily added by an admin (a CLI action), but can also
//! onboard through the browser by proving control of a passkey — no push key
//! required. `Trust::WebAuthn` records that credential set, but authorizes web
//! sign-in only: [`member_lines`] and [`Member::keys`] emit nothing for it, so
//! such a member cannot push at all. `provenance` separately records whether
//! the member was admin-registered or self-attested via the web, so the web
//! layer can grant a self-attested member only limited trust until an admin
//! promotes them (`pre_receive` stays purely key-based and is unaffected,
//! since a self-attested member has no push key to gate anyway).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use facet::Facet;

/// The namespace whose refs hold the member set — the push trust root. One
/// `refs/meta/member/<username>` ref per person.
pub const MEMBER_NS: &str = "refs/meta/member";

/// The ref holding the member named `username`.
#[must_use]
pub fn member_ref(username: &str) -> String {
    format!("{MEMBER_NS}/{username}")
}

/// One member: a person named by their `refs/meta/member/<principal>` ref, the
/// window their trust holds within, and the keys (or, later, CA) it rests on.
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct Member {
    /// The member's username — the ref's last segment, and the `allowed_signers`
    /// principal once identities are bound to signing keys.
    pub principal: String,
    /// The OpenSSH timestamp (`YYYYMMDD[Z]` or `YYYYMMDDHHMM[SS][Z]`) at or after
    /// which the member is trusted, or `None` for no lower bound.
    pub valid_after: Option<String>,
    /// The OpenSSH timestamp at or before which the member is trusted, or `None`
    /// for trust that never lapses on its own.
    pub valid_before: Option<String>,
    /// What the member's trust rests on.
    pub trust: Trust,
    /// Whether the member was admin-registered or self-attested via web
    /// onboarding.
    ///
    /// `#[facet(default)]` is required (not just `Option`, which auto-defaults
    /// on its own): without it, and without `Provenance: Default`, a member
    /// ref written before this field existed would fail to load, `load_all`
    /// would error, and `pre_receive` would refuse every push. See
    /// `loads_a_member_ref_with_no_provenance_entry`.
    #[facet(default)]
    pub provenance: Provenance,
    /// The `@`-mentioned account this member is, by its stable
    /// [`crate::account::genesis`] hash — `None` until an admin links one.
    /// Plain `Option`, which `facet-git-tree` auto-defaults on an absent
    /// entry, so a member ref written before this field existed keeps
    /// loading unchanged.
    pub account: Option<String>,
}

/// Whether a member was admin-registered or self-attested via web onboarding.
/// Defaults to [`Provenance::AdminRegistered`] so a member ref written before
/// this field existed loads unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Facet)]
#[repr(u8)]
pub enum Provenance {
    /// Added by an admin (a CLI action). Fully trusted.
    #[default]
    AdminRegistered,
    /// Onboarded through the browser by proving a passkey, with no admin
    /// action. Limited trust until an admin promotes them — enforced by the
    /// web layer, not `pre_receive`.
    SelfAttestedWeb,
}

/// A WebAuthn passkey credential, in COSE form, with a human-readable label.
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct WebAuthnKey {
    /// The credential's public key, in COSE form.
    pub cose_key: String,
    /// A human-readable label (e.g. the authenticator's name).
    pub label: String,
}

/// What a member's trust rests on: a set of leaf keys, a pinned certificate
/// authority, or a set of WebAuthn passkeys — additive cases, not a migration
/// of one another. Pinning a CA decouples the stable pin from ephemeral device
/// keys, so rotation, expiry, and new devices cost zero downstream edits; it is
/// a security win only when the CA lives off the device (hardware token,
/// offline, or a remote issuer behind SSO).
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
#[repr(u8)]
pub enum Trust {
    /// A set of leaf signing keys, mapping each fingerprint to its OpenSSH public
    /// key. The solo/small-team default.
    Keys(BTreeMap<String, String>),
    /// A pinned certificate authority's OpenSSH public key: any certificate it
    /// issues for the member's principal, within the cert's own validity window,
    /// is trusted. The enterprise / many-repos option.
    CertAuthority(String),
    /// A set of passkey credentials, keyed by credential id. Authorizes browser
    /// sign-in only: [`member_lines`] and [`Member::keys`] emit nothing for it,
    /// so a `WebAuthn` member cannot push at all.
    WebAuthn(BTreeMap<String, WebAuthnKey>),
}

impl git_store::HasId for Member {
    fn id(&self) -> &str {
        &self.principal
    }
}

impl iddqd::IdOrdItem for Member {
    type Key<'a> = &'a str;

    fn key(&self) -> Self::Key<'_> {
        &self.principal
    }

    iddqd::id_upcast!();
}

impl Member {
    /// A member trusting `keys` with no validity window, admin-registered.
    #[must_use]
    pub fn with_keys(principal: String, keys: BTreeMap<String, String>) -> Self {
        Self {
            principal,
            valid_after: None,
            valid_before: None,
            trust: Trust::Keys(keys),
            provenance: Provenance::AdminRegistered,
            account: None,
        }
    }

    /// A member trusting any certificate the CA `ca` issues for them, with no
    /// validity window, admin-registered.
    #[must_use]
    pub fn with_ca(principal: String, ca: String) -> Self {
        Self {
            principal,
            valid_after: None,
            valid_before: None,
            trust: Trust::CertAuthority(ca),
            provenance: Provenance::AdminRegistered,
            account: None,
        }
    }

    /// A member trusting `keys` (a WebAuthn credential set) with no validity
    /// window, self-attested via web onboarding.
    #[must_use]
    pub fn with_webauthn(principal: String, keys: BTreeMap<String, WebAuthnKey>) -> Self {
        Self {
            principal,
            valid_after: None,
            valid_before: None,
            trust: Trust::WebAuthn(keys),
            provenance: Provenance::SelfAttestedWeb,
            account: None,
        }
    }

    /// The member's leaf signing keys as `(fingerprint, key)` pairs. A member
    /// resting on a CA or WebAuthn credentials has no leaf keys and yields
    /// none — a `WebAuthn` member authorizes web sign-in only and cannot push.
    #[must_use]
    pub fn keys(&self) -> Vec<(&String, &String)> {
        match &self.trust {
            Trust::Keys(keys) => keys.iter().collect(),
            Trust::CertAuthority(_) | Trust::WebAuthn(_) => Vec::new(),
        }
    }

    /// The member's pinned certificate authority key, or `None` when the member
    /// rests on leaf keys or WebAuthn credentials.
    #[must_use]
    pub fn ca(&self) -> Option<&str> {
        match &self.trust {
            Trust::CertAuthority(ca) => Some(ca),
            Trust::Keys(_) | Trust::WebAuthn(_) => None,
        }
    }

    /// Check invariants the type system does not enforce: a set validity
    /// bound must be a well-formed OpenSSH timestamp, and when both bounds
    /// are set, `valid_after` must not be after `valid_before` — an inverted
    /// window would authorize nothing, silently locking every one of the
    /// member's keys out rather than the admin's intended restriction.
    /// [`store`] checks this before every write, so it holds regardless of
    /// which caller builds the member (the CLI today, an admin web action
    /// later).
    pub fn validate(&self) -> Result<(), String> {
        for bound in [&self.valid_after, &self.valid_before] {
            if let Some(value) = bound
                && !valid_timestamp(value)
            {
                return Err(format!(
                    "{value:?} is not a valid OpenSSH timestamp \
                     (expected YYYYMMDD[Z] or YYYYMMDDHHMM[SS][Z])"
                ));
            }
        }
        if let (Some(after), Some(before)) = (&self.valid_after, &self.valid_before)
            && timestamp_key(after) > timestamp_key(before)
        {
            return Err(format!(
                "valid-after {after:?} is after valid-before {before:?}: \
                 this window would never authorize a push"
            ));
        }
        Ok(())
    }
}

/// Whether `value` is a well-formed OpenSSH `allowed_signers` timestamp:
/// `YYYYMMDD`, `YYYYMMDDHHMM`, or `YYYYMMDDHHMMSS`, each optionally suffixed
/// `Z` for UTC. Without `Z` the verifying server reads it in its own local
/// time zone.
#[must_use]
pub fn valid_timestamp(value: &str) -> bool {
    let digits = value.strip_suffix('Z').unwrap_or(value);
    matches!(digits.len(), 8 | 12 | 14) && digits.bytes().all(|b| b.is_ascii_digit())
}

/// `value`'s digits, right-padded to 14 (`YYYYMMDDHHMMSS`), so two timestamps
/// of different precision compare correctly as calendar time. Ignores any `Z`
/// suffix — comparing a UTC bound against a local one is inherently
/// approximate; exact time-zone arithmetic is out of scope for this ordering
/// check.
fn timestamp_key(value: &str) -> String {
    let digits = value.strip_suffix('Z').unwrap_or(value);
    format!("{digits:0<14}")
}

/// Load the member named `username` from an already-open `store`.
pub fn load_with(
    store: &git_store::Store,
    username: &str,
) -> Result<Option<Member>, git_store::Error> {
    store.load_item(MEMBER_NS, username)
}

/// Load the member named `username` in `repo`, or `None` when the ref is absent.
pub fn load(repo: &Path, username: &str) -> Result<Option<Member>, git_store::Error> {
    load_with(&git_store::Store::open(repo)?, username)
}

/// Load every member recorded under [`MEMBER_NS`] from an already-open
/// `store`, newest ref first.
///
/// An empty result is a fresh server whose trust list has not been pushed yet. A
/// present but unreadable member ref is an error so callers can fail closed
/// rather than mistake corruption for "no members".
pub fn load_all_with(store: &git_store::Store) -> Result<Vec<Member>, git_store::Error> {
    Ok(store
        .list_items::<Member>(MEMBER_NS)?
        .into_iter()
        .map(|(_id, member)| member)
        .collect())
}

/// Load every member recorded under [`MEMBER_NS`] in `repo`. See
/// [`load_all_with`].
pub fn load_all(repo: &Path) -> Result<Vec<Member>, git_store::Error> {
    load_all_with(&git_store::Store::open(repo)?)
}

/// Load every member recorded under [`MEMBER_NS`] in `repo`, keyed by
/// principal.
///
/// Prepares the batch path for lookups keyed by principal directly (there is
/// exactly one member per principal, unlike a signing key, which a member may
/// legitimately hold several of — that is why this indexes principals rather
/// than a `Trust::Keys` bi-map). An [`iddqd::IdOrdMap`] rather than a
/// `BTreeMap<String, Member>` so the principal lives once, on `Member`
/// itself, instead of also duplicated as a separately-maintained map key. Not
/// yet wired to any caller: the web layer's public-key lookup needs a
/// different index (key → member), an O(m×k) linear scan that stays fine at
/// current scale (see its own doc comment).
pub fn load_all_indexed(repo: &Path) -> Result<iddqd::IdOrdMap<Member>, git_store::Error> {
    Ok(load_all(repo)?.into_iter().collect())
}

/// Write `member` to its `refs/meta/member/<principal>` ref in `repo`,
/// replacing any prior value, as a new commit. Rejects a member whose
/// validity window is malformed or inverted — see [`Member::validate`].
pub fn store(repo: &Path, member: &Member) -> Result<(), git_store::Error> {
    member.validate().map_err(git_store::Error::Invalid)?;
    git_store::Store::open(repo)?.store_keyed(MEMBER_NS, member, "Update member")
}

/// Drop every `revoked` fingerprint from `members`, returning the trust set the
/// verifier should actually honor.
///
/// A leaf key whose fingerprint is revoked is removed; a member left with no keys
/// drops out entirely, so a push it would have authorized fails closed. A member
/// resting on a CA is untouched — a compromised CA is revoked by removing its
/// member ref, since a CA is named by a ref rather than listed by fingerprint.
#[must_use]
pub fn without_revoked(members: Vec<Member>, revoked: &BTreeSet<String>) -> Vec<Member> {
    members
        .into_iter()
        .filter_map(|mut member| match &mut member.trust {
            Trust::Keys(keys) => {
                keys.retain(|fingerprint, _key| !revoked.contains(fingerprint));
                if keys.is_empty() { None } else { Some(member) }
            }
            Trust::CertAuthority(_) | Trust::WebAuthn(_) => Some(member),
        })
        .collect()
}

/// Render `members` as an OpenSSH `allowed_signers` file that authorizes any
/// pusher identity (`*`) signing in git's namespace.
///
/// The principal is a wildcard because authentication here is membership of the
/// key set, not a binding between a key and a particular identity: `ssh-keygen
/// -Y verify` accepts the push certificate as long as the signing key is one of
/// these, whatever name the pusher signed under. The member's username lives in
/// the ref and the `principal` field; binding it to the signing principal is a
/// later identity concern.
///
/// Each member's `valid-after`/`valid-before` window is rendered as
/// `allowed_signers` options so git enforces expiry: out-of-window keys are not
/// accepted. Options are comma-joined, the syntax OpenSSH requires for more than
/// one.
#[must_use]
pub fn allowed_signers(members: &[Member]) -> String {
    members.iter().flat_map(member_lines).collect::<String>()
}

/// The `allowed_signers` lines for one member: one per leaf key, a single
/// `cert-authority` line for a pinned CA, or none at all for `WebAuthn`
/// credentials, which authorize browser sign-in only. Each key/CA line carries
/// the member's validity window. `ssh-keygen -Y verify` consumes the
/// `cert-authority` flag natively — it accepts a certificate the CA issued for
/// the verified principal — so no special verifier logic is needed beyond
/// emitting the line.
fn member_lines(member: &Member) -> Vec<String> {
    match &member.trust {
        Trust::Keys(keys) => keys
            .values()
            .map(|key| allowed_signers_line(member, false, key))
            .collect(),
        Trust::CertAuthority(ca) => vec![allowed_signers_line(member, true, ca)],
        Trust::WebAuthn(_) => Vec::new(),
    }
}

/// One `allowed_signers` line: the wildcard principal, the `cert-authority` flag
/// when `ca` is set, the member's validity window, the git namespace, and the
/// key. Options are comma-joined, the syntax OpenSSH requires for more than one.
fn allowed_signers_line(member: &Member, ca: bool, key: &str) -> String {
    let mut options = Vec::new();
    if ca {
        options.push("cert-authority".to_owned());
    }
    if let Some(after) = &member.valid_after {
        options.push(format!("valid-after=\"{after}\""));
    }
    if let Some(before) = &member.valid_before {
        options.push(format!("valid-before=\"{before}\""));
    }
    options.push("namespaces=\"git\"".to_owned());
    format!("* {} {}\n", options.join(","), key)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::let_underscore_must_use,
        reason = "unit test"
    )]

    use super::*;
    use crate::testutil::{unique_repo as new_repo, write_member_doc, write_webauthn_member_doc};

    const KEY_A: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaA alice";
    const KEY_B: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbB bob";

    fn unique_repo() -> std::path::PathBuf {
        new_repo("signers")
    }

    fn keys(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(fp, key)| ((*fp).to_owned(), (*key).to_owned()))
            .collect()
    }

    #[test]
    fn store_then_load_round_trips_a_member() {
        let repo = unique_repo();
        let mut member = Member::with_keys(
            "alice".to_owned(),
            keys(&[("aa:bb", KEY_A), ("cc:dd", KEY_B)]),
        );
        member.valid_after = Some("20260101".to_owned());
        member.valid_before = Some("20270101".to_owned());
        store(&repo, &member).unwrap();

        assert_eq!(load(&repo, "alice").unwrap(), Some(member));
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn load_all_unions_every_member_ref() {
        let repo = unique_repo();
        store(
            &repo,
            &Member::with_keys("alice".to_owned(), keys(&[("aa:bb", KEY_A)])),
        )
        .unwrap();
        store(
            &repo,
            &Member::with_keys("bob".to_owned(), keys(&[("cc:dd", KEY_B)])),
        )
        .unwrap();

        let mut principals: Vec<String> = load_all(&repo)
            .unwrap()
            .into_iter()
            .map(|member| member.principal)
            .collect();
        principals.sort();
        assert_eq!(principals, vec!["alice".to_owned(), "bob".to_owned()]);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn empty_when_no_member_refs_exist() {
        let repo = unique_repo();
        assert!(load_all(&repo).unwrap().is_empty());
        assert_eq!(load(&repo, "nobody").unwrap(), None);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn loads_the_on_disk_member_format_with_no_provenance_entry_as_admin_registered() {
        // A fixture written as the real `member/<username>` layout — a `principal`
        // blob, `valid_after`/`valid_before` Option subtrees, and a
        // `trust/Keys/<fingerprint>` subtree, with no `provenance` entry at all —
        // must keep loading, and load as `AdminRegistered`. Without
        // `#[facet(default)]` on `provenance` this fixture would fail to
        // deserialize, `load_all` would error, and `pre_receive` would refuse
        // every push: this is the regression guard for that.
        let repo = unique_repo();
        write_member_doc(
            &repo,
            "alice",
            None,
            Some("20270101"),
            &[("aa:bb:cc", KEY_A)],
        );
        let member = load(&repo, "alice").unwrap().unwrap();
        assert_eq!(member.principal, "alice");
        assert_eq!(member.valid_before, Some("20270101".to_owned()));
        assert_eq!(
            member.keys(),
            vec![(&"aa:bb:cc".to_owned(), &KEY_A.to_owned())]
        );
        assert_eq!(member.provenance, Provenance::AdminRegistered);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn loads_a_hand_built_webauthn_fixture() {
        // A hand-built `trust/WebAuthn/<credential_id>/{cose_key,label}`
        // fixture with an explicit `provenance/SelfAttestedWeb` entry must
        // round-trip; a `Keys`/`CertAuthority` fixture (the prior two tests)
        // must keep loading unchanged alongside the new variant.
        let repo = unique_repo();
        write_webauthn_member_doc(
            &repo,
            "alice",
            "SelfAttestedWeb",
            &[("cred-1", "cose-bytes", "YubiKey")],
        );
        let member = load(&repo, "alice").unwrap().unwrap();
        assert_eq!(member.principal, "alice");
        assert_eq!(member.provenance, Provenance::SelfAttestedWeb);
        assert!(member.keys().is_empty());
        assert_eq!(member.ca(), None);
        assert_eq!(
            member.trust,
            Trust::WebAuthn(BTreeMap::from([(
                "cred-1".to_owned(),
                WebAuthnKey {
                    cose_key: "cose-bytes".to_owned(),
                    label: "YubiKey".to_owned(),
                }
            )]))
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn store_then_load_round_trips_a_webauthn_member() {
        let repo = unique_repo();
        let creds = BTreeMap::from([(
            "cred-1".to_owned(),
            WebAuthnKey {
                cose_key: "cose-bytes".to_owned(),
                label: "YubiKey".to_owned(),
            },
        )]);
        let member = Member::with_webauthn("alice".to_owned(), creds);
        store(&repo, &member).unwrap();
        let loaded = load(&repo, "alice").unwrap().unwrap();
        assert_eq!(loaded, member);
        assert_eq!(loaded.provenance, Provenance::SelfAttestedWeb);
        // A WebAuthn member authorizes web sign-in only: no allowed_signers
        // line, so it cannot push at all.
        assert!(loaded.keys().is_empty());
        assert!(allowed_signers(&[loaded]).is_empty());
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn renders_a_wildcard_allowed_signers_file() {
        let member = Member::with_keys("alice".to_owned(), keys(&[("aa:bb", KEY_A)]));
        assert_eq!(
            allowed_signers(&[member]),
            format!("* namespaces=\"git\" {KEY_A}\n")
        );
    }

    #[test]
    fn renders_the_validity_window_as_comma_joined_options() {
        let mut member = Member::with_keys("alice".to_owned(), keys(&[("aa:bb", KEY_A)]));
        member.valid_after = Some("20260101".to_owned());
        member.valid_before = Some("20270101".to_owned());
        assert_eq!(
            allowed_signers(&[member]),
            format!(
                "* valid-after=\"20260101\",valid-before=\"20270101\",namespaces=\"git\" {KEY_A}\n"
            )
        );
    }

    #[test]
    fn store_then_load_round_trips_a_ca_member() {
        let repo = unique_repo();
        let member = Member::with_ca("alice".to_owned(), KEY_A.to_owned());
        store(&repo, &member).unwrap();
        let loaded = load(&repo, "alice").unwrap().unwrap();
        assert_eq!(loaded, member);
        assert_eq!(loaded.ca(), Some(KEY_A));
        assert!(loaded.keys().is_empty());
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn without_revoked_drops_revoked_keys_and_emptied_members() {
        let alice = Member::with_keys(
            "alice".to_owned(),
            keys(&[("aa:bb", KEY_A), ("cc:dd", KEY_B)]),
        );
        let bob = Member::with_keys("bob".to_owned(), keys(&[("ee:ff", KEY_A)]));
        let revoked = BTreeSet::from(["cc:dd".to_owned(), "ee:ff".to_owned()]);

        // bob's only key was revoked, so bob drops out entirely; alice keeps her
        // un-revoked key.
        let alice_kept = Member::with_keys("alice".to_owned(), keys(&[("aa:bb", KEY_A)]));
        assert_eq!(
            without_revoked(vec![alice, bob], &revoked),
            vec![alice_kept]
        );
    }

    #[test]
    fn without_revoked_leaves_ca_members_untouched() {
        let member = Member::with_ca("alice".to_owned(), KEY_A.to_owned());
        let revoked = BTreeSet::from(["aa:bb".to_owned()]);
        assert_eq!(
            without_revoked(vec![member.clone()], &revoked),
            vec![member]
        );
    }

    #[test]
    fn renders_a_pinned_ca_as_a_cert_authority_line() {
        let mut member = Member::with_ca("alice".to_owned(), KEY_A.to_owned());
        member.valid_before = Some("20270101".to_owned());
        assert_eq!(
            allowed_signers(&[member]),
            format!("* cert-authority,valid-before=\"20270101\",namespaces=\"git\" {KEY_A}\n")
        );
    }

    #[test]
    fn validate_rejects_a_malformed_timestamp() {
        let mut member = Member::with_keys("alice".to_owned(), keys(&[("aa:bb", KEY_A)]));
        member.valid_before = Some("not-a-timestamp".to_owned());
        assert!(member.validate().is_err());
    }

    #[test]
    fn validate_rejects_an_inverted_window() {
        let mut member = Member::with_keys("alice".to_owned(), keys(&[("aa:bb", KEY_A)]));
        member.valid_after = Some("20270101".to_owned());
        member.valid_before = Some("20260101".to_owned());
        assert!(member.validate().is_err());
    }

    #[test]
    fn validate_accepts_an_ordered_window_of_mixed_precision() {
        let mut member = Member::with_keys("alice".to_owned(), keys(&[("aa:bb", KEY_A)]));
        member.valid_after = Some("20260101".to_owned());
        member.valid_before = Some("20270101120000Z".to_owned());
        member.validate().unwrap();
    }

    #[test]
    fn store_rejects_a_member_with_an_inverted_window() {
        let repo = unique_repo();
        let mut member = Member::with_keys("alice".to_owned(), keys(&[("aa:bb", KEY_A)]));
        member.valid_after = Some("20270101".to_owned());
        member.valid_before = Some("20260101".to_owned());
        let result = store(&repo, &member);
        assert!(matches!(result, Err(git_store::Error::Invalid(_))));
        assert_eq!(load(&repo, "alice").unwrap(), None);
        let _ = std::fs::remove_dir_all(&repo);
    }
}
