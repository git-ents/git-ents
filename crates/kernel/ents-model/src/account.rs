//! The Account entity: links a member's key to a login identity.
//!
//! Spec coverage: `model.account`.

use facet::Facet;

use crate::member::MemberId;

/// Links a member to a login identity, living at the fixed
/// `refs/meta/account` ref (`namespace::ACCOUNT_REF`,
/// `meta-ref.granularity`: repository-global state with a single
/// writer-of-record lives on one fixed ref, not one ref per entity).
///
/// `model.account` requires authentication state to live in the
/// repository as ordinary forge state, never a session database or token
/// table — this struct is that state, nothing more: which member the
/// account belongs to, and the login identity it maps to. What that login
/// identity looks like (an email, an OAuth subject, a passkey credential
/// id) is left to whatever frontend authenticates against it; `ents-model`
/// does not constrain its format.
///
/// # Examples
///
/// ```
/// use ents_model::{Account, MemberId};
///
/// let account = Account {
///     member: MemberId::new("jdc"),
///     login: "joseph.carpinelli@icloud.com".to_owned(),
/// };
/// let (id, store) = facet_git_tree::serialize(&account).expect("serialize");
/// let back: Account = facet_git_tree::deserialize(&id, &store).expect("deserialize");
/// assert_eq!(back, account);
/// ```
// @relation(model.account, meta-ref.typed-tree, model.extensibility, scope=file)
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct Account {
    /// The member this account belongs to.
    pub member: MemberId,
    /// The login identity the member authenticates as.
    pub login: String,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "unit test")]

    use facet_git_tree::{deserialize, serialize};
    use rstest::rstest;

    use super::*;

    #[rstest]
    // @relation(model.account, meta-ref.typed-tree, scope=function, role=Verifies)
    fn account_round_trips_through_a_tree() {
        let account = Account {
            member: MemberId::new("jdc"),
            login: "joseph.carpinelli@icloud.com".to_owned(),
        };
        let (id, store) = serialize(&account).expect("serialize");
        let back: Account = deserialize(&id, &store).expect("deserialize");
        assert_eq!(back, account);
    }
}
