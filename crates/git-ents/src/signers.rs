//! The repository's members, sourced from the `refs/meta/member/*` refs.
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

use std::collections::BTreeMap;
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
}

/// What a member's trust rests on. A member is *either* a set of leaf keys *or*
/// (from Phase 3) a pinned certificate authority — additive cases, not a
/// migration of one another.
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
#[repr(u8)]
pub enum Trust {
    /// A set of leaf signing keys, mapping each fingerprint to its OpenSSH public
    /// key.
    Keys(BTreeMap<String, String>),
}

impl Member {
    /// A member trusting `keys` with no validity window.
    #[must_use]
    pub fn with_keys(principal: String, keys: BTreeMap<String, String>) -> Self {
        Self {
            principal,
            valid_after: None,
            valid_before: None,
            trust: Trust::Keys(keys),
        }
    }

    /// The member's leaf signing keys as `(fingerprint, key)` pairs. A member
    /// resting on a CA (Phase 3) has no leaf keys and yields none.
    #[must_use]
    pub fn keys(&self) -> Vec<(&String, &String)> {
        match &self.trust {
            Trust::Keys(keys) => keys.iter().collect(),
        }
    }
}

/// Load the member named `username` in `repo`, or `None` when the ref is absent.
pub fn load(repo: &Path, username: &str) -> Result<Option<Member>, git_store::Error> {
    git_store::Store::open(repo)?.load::<Member>(&member_ref(username))
}

/// Load every member recorded under [`MEMBER_NS`] in `repo`, newest ref first.
///
/// An empty result is a fresh server whose trust list has not been pushed yet. A
/// present but unreadable member ref is an error so callers can fail closed
/// rather than mistake corruption for "no members".
pub fn load_all(repo: &Path) -> Result<Vec<Member>, git_store::Error> {
    let store = git_store::Store::open(repo)?;
    let mut members = Vec::new();
    for refname in store.list(&format!("{MEMBER_NS}/"))? {
        if let Some(member) = store.load::<Member>(&refname)? {
            members.push(member);
        }
    }
    Ok(members)
}

/// Write `member` to its `refs/meta/member/<principal>` ref, replacing any prior
/// value, as a new commit.
pub fn store(repo: &Path, member: &Member) -> Result<(), git_store::Error> {
    git_store::Store::open(repo)?.store(&member_ref(&member.principal), member, "Update member")?;
    Ok(())
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

/// The `allowed_signers` lines for one member: one per leaf key, each carrying
/// the member's validity window.
fn member_lines(member: &Member) -> Vec<String> {
    member
        .keys()
        .into_iter()
        .map(|(_fingerprint, key)| allowed_signers_line(member, key))
        .collect()
}

/// One `allowed_signers` line: the wildcard principal, the member's validity
/// window, the git namespace, and the key.
fn allowed_signers_line(member: &Member, key: &str) -> String {
    let mut options = Vec::new();
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
    use crate::testutil::{unique_repo as new_repo, write_member_doc};

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
    fn loads_the_on_disk_member_format() {
        // A fixture written as the real `member/<username>` layout — a `principal`
        // blob, `valid_after`/`valid_before` Option subtrees, and a
        // `trust/Keys/<fingerprint>` subtree — must keep loading; this fails if
        // the Member document's shape changes incompatibly with data on a ref.
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
}
