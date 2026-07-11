//! `git ents account create`: link a member to a login identity at the
//! fixed `refs/meta/account` ref (`model.account`).

use ents_model::{Account, MemberId, namespace};
use gix_ref_store::RefStoreRead;

use super::{actor, signer};
use crate::error::{Error, Result};
use crate::mutate::{Identity, outcome_to_result, propose_entity};
use crate::root::LocalRoot;

/// `git ents account show`: this repository's account identity.
///
/// # Errors
///
/// [`Error::NotFound`] if no account has been created yet
/// (`git ents account create` first).
pub fn show(root: &LocalRoot) -> Result<Account> {
    #[expect(
        clippy::expect_used,
        clippy::unwrap_in_result,
        reason = "ACCOUNT_REF is a fixed, compile-time-known-valid refname literal"
    )]
    let name: gix::refs::FullName = namespace::ACCOUNT_REF
        .try_into()
        .expect("fixed, valid refname");
    let Some(tip) = root.refs.get(name.as_ref())? else {
        return Err(Error::NotFound {
            what: "account".to_owned(),
        });
    };
    let tree = super::commit_tree(&root.objects, tip)?;
    Ok(facet_git_tree::deserialize::<Account>(
        &tree,
        &root.objects,
    )?)
}

/// Run `git ents account create`.
///
/// # Errors
///
/// [`Error::NotFound`] if `member` is given but no such member exists (or,
/// when omitted, the signer's own key is not enrolled yet — enroll it with
/// `git ents members add` first); otherwise see
/// [`crate::mutate::outcome_to_result`].
pub fn create(
    root: &LocalRoot,
    member: Option<String>,
    login: String,
    key: Option<std::path::PathBuf>,
) -> Result<()> {
    let signer = signer(root, key)?;
    let member_id = match member {
        Some(username) => MemberId::new(username),
        None => {
            let (username, _) =
                super::members::check(root, None)?.ok_or_else(|| Error::NotFound {
                    what: "member for the current signing key".to_owned(),
                })?;
            MemberId::new(username)
        }
    };
    let account = Account {
        member: member_id,
        login,
    };
    #[expect(
        clippy::expect_used,
        clippy::unwrap_in_result,
        reason = "ACCOUNT_REF is a fixed, compile-time-known-valid refname literal"
    )]
    let name: gix::refs::FullName = namespace::ACCOUNT_REF
        .try_into()
        .expect("fixed, valid refname");
    let identity = Identity {
        actor: actor(&signer),
        signer: &signer,
    };
    let outcome = propose_entity(
        &root.refs,
        &root.objects,
        &root.events,
        name,
        &account,
        &identity,
        "Create account",
        root.mode(),
    )?;
    outcome_to_result(outcome, None)?;
    Ok(())
}
