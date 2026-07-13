//! `git ents members`: enroll, remove, revoke, unrevoke, and check members
//! (`model.member-identity`, `model.member-revocation`).

use ents_model::{Member, MemberId, MemberState, Provenance, namespace};
use ents_receive::{Identity, propose_delete, propose_entity};
use gix_ref_store::RefStoreRead;

use super::{actor, signer};
use crate::error::{Error, Result};
use crate::mutate::outcome_to_result;
use crate::root::LocalRoot;

/// `git ents members list`: every member ref and its current state.
///
/// # Errors
///
/// Propagates a ref-store or object read failure.
pub fn list(root: &LocalRoot) -> Result<Vec<(String, Member)>> {
    let mut out = Vec::new();
    for entry in root.refs.iter_prefix("refs/meta/member/")? {
        let (name, tip) = entry?;
        let path = name.as_bstr().to_string();
        let Some(username) = path.strip_prefix("refs/meta/member/") else {
            continue;
        };
        if let Some(member) = read_member(root, tip)? {
            out.push((username.to_owned(), member));
        }
    }
    Ok(out)
}

/// `git ents members add`: enroll `username` with `pubkey` (or the
/// signer's own public key), admin-registered.
///
/// # Errors
///
/// Propagates a signing, serialization, or `receive` failure; see
/// [`crate::mutate::outcome_to_result`] for how a reached refusal renders.
pub fn add(
    root: &LocalRoot,
    username: &str,
    pubkey: Option<String>,
    key: Option<std::path::PathBuf>,
) -> Result<()> {
    let signer = signer(root, key)?;
    let pubkey = pubkey.unwrap_or_else(|| signer.public_openssh());
    let member = Member::new(MemberId::new(username), pubkey, Provenance::AdminRegistered);
    let name = namespace::member_ref(&MemberId::new(username))?;
    let identity = Identity {
        actor: actor(&signer),
        sign: &|payload| signer.sign(payload),
    };
    let outcome = propose_entity(
        &root.refs,
        &root.objects,
        &root.events,
        name,
        &member,
        &identity,
        &format!("Enroll {username}"),
        root.mode(),
    )?;
    outcome_to_result(outcome, None)?;
    Ok(())
}

/// `git ents members remove`: delete `username`'s ref entirely.
///
/// # Errors
///
/// See [`add`].
pub fn remove(root: &LocalRoot, username: &str, key: Option<std::path::PathBuf>) -> Result<()> {
    let signer = signer(root, key)?;
    let name = namespace::member_ref(&MemberId::new(username))?;
    let outcome = propose_delete(&root.refs, &root.objects, &root.events, name, root.mode())?;
    let _ = signer; // signing material is not needed for a deletion transition.
    outcome_to_result(outcome, None)?;
    Ok(())
}

/// `git ents members revoke`/`unrevoke`: flip `username`'s
/// [`MemberState`] without deleting the record (`model.member-revocation`).
///
/// # Errors
///
/// [`Error::NotFound`] if `username` has no member ref; otherwise see
/// [`add`].
pub fn set_revoked(
    root: &LocalRoot,
    username: &str,
    revoked: bool,
    key: Option<std::path::PathBuf>,
) -> Result<()> {
    let signer = signer(root, key)?;
    let name = namespace::member_ref(&MemberId::new(username))?;
    let Some(tip) = root.refs.get(name.as_ref())? else {
        return Err(Error::NotFound {
            what: format!("member {username}"),
        });
    };
    let mut member = read_member(root, tip)?.ok_or_else(|| Error::NotFound {
        what: format!("member {username}"),
    })?;
    member.state = if revoked {
        MemberState::Revoked
    } else {
        MemberState::Active
    };
    let identity = Identity {
        actor: actor(&signer),
        sign: &|payload| signer.sign(payload),
    };
    let verb = if revoked { "Revoke" } else { "Unrevoke" };
    let outcome = propose_entity(
        &root.refs,
        &root.objects,
        &root.events,
        name,
        &member,
        &identity,
        &format!("{verb} {username}"),
        root.mode(),
    )?;
    outcome_to_result(outcome, Some(tip))?;
    Ok(())
}

/// `git ents members check`: whether `key` (or the resolved signing key)
/// names an active member, and which username.
///
/// # Errors
///
/// Propagates a signing-key or ref-store read failure.
pub fn check(
    root: &LocalRoot,
    key: Option<std::path::PathBuf>,
) -> Result<Option<(String, MemberState)>> {
    let signer = signer(root, key)?;
    find_by_key(root, &signer.public_openssh())
}

/// Resolve `pubkey` to the enrolled member whose stored key matches it, if
/// any -- the shared match loop behind [`check`] and `git ents serve`'s own
/// identity-chip label (`crate::commands::serve::build_state`,
/// `roots.web-signing`): both need "which member owns this key," never a
/// bespoke re-scan of `list`'s own rows.
///
/// # Errors
///
/// Propagates a ref-store or object read failure.
pub fn find_by_key(root: &LocalRoot, pubkey: &str) -> Result<Option<(String, MemberState)>> {
    for (username, member) in list(root)? {
        if member.key == pubkey {
            return Ok(Some((username, member.state)));
        }
    }
    Ok(None)
}

fn read_member(root: &LocalRoot, tip: gix_hash::ObjectId) -> Result<Option<Member>> {
    let tree = crate::commands::commit_tree(&root.objects, tip)?;
    Ok(facet_git_tree::deserialize::<Member>(&tree, &root.objects).ok())
}
