//! Policy loading: the member set, read from `refs/meta/member/*`
//! through the read half of the ref store (`gate.policy-as-state`).
//!
//! The gate consults no state outside `refs/meta/*`: the member set,
//! each member's current state, and the epoch (`crate::config`) are all
//! repository state, so any frontend with a clone evaluates the actual
//! policy offline, staleness bounded only by the age of its last fetch.

use ents_model::{Member, MemberId};
use gix_hash::ObjectId;
use gix_object::Find;
use gix_ref_store::RefStoreRead;

use crate::error::{Error, Result};
use crate::object::expect_commit;

/// One enrolled member: its id (from the refname) and its ref's tip.
#[derive(Debug, Clone)]
pub(crate) struct Enrolled {
    /// The member id, i.e. the `<id>` of `refs/meta/member/<id>`.
    pub id: MemberId,
    /// The member ref's current tip commit.
    pub tip: ObjectId,
}

/// Every ref under `refs/meta/member/`, in store order.
// @relation(gate.policy-as-state, scope=function)
pub(crate) fn members(refs: &dyn RefStoreRead) -> Result<Vec<Enrolled>> {
    let mut out = Vec::new();
    for entry in refs.iter_prefix("refs/meta/member/")? {
        let (name, tip) = entry?;
        let path = name.as_bstr().to_string();
        let id = path
            .strip_prefix("refs/meta/member/")
            .unwrap_or(&path)
            .to_owned();
        out.push(Enrolled {
            id: MemberId::new(id),
            tip,
        });
    }
    Ok(out)
}

/// The member entity currently in force: the typed tree behind `tip`,
/// the member ref's tip as read in the same verification snapshot.
///
/// Admission consults only this current entity
/// (`model.member-revocation`): a revoked key's new pushes are refused
/// from the moment the revocation lands, regardless of any committer
/// timestamp the pushed commit claims — a backdated commit changes
/// nothing, because no commit-supplied time participates in the
/// judgment. Refs accepted before a revocation stay valid because
/// acceptance is never re-judged; reconstructing what a past acceptance
/// saw is an audit function over the deployment's out-of-scope op log,
/// not a gate path.
// @relation(model.member-revocation, gate.policy-as-state, scope=function)
pub(crate) fn member_current(objects: &dyn Find, tip: ObjectId) -> Result<Member> {
    let commit = expect_commit(objects, tip)?;
    facet_git_tree::deserialize(&commit.tree, objects)
        .map_err(|source| Error::Entity { oid: tip, source })
}
