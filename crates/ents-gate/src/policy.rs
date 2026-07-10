//! Policy loading: the member set, read from `refs/meta/member/*`
//! through the read half of the ref store (`gate.policy-as-state`).
//!
//! The gate consults no state outside `refs/meta/*`: members, their
//! revocation timelines, and the epoch (`crate::config`) are all
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

/// The member entity in force at `at_seconds`, found by walking the
/// member ref's own commit history (first-parent) from `tip` back to the
/// newest mutation at or before that time, and deserializing *that*
/// commit's tree.
///
/// This is how revocation gets its before/after boundary with no
/// validity-window field on the entity (`model.member-revocation`): the
/// ref's commit chain is the audit trail (`meta-ref.namespace`), so the
/// state, provenance, and key that judge a signature are the ones the
/// chain records for the signature's own timestamp. `Ok(None)` means the
/// member had not been enrolled yet at `at_seconds`.
// @relation(model.member-revocation, gate.policy-as-state, scope=function)
pub(crate) fn member_at(
    objects: &dyn Find,
    tip: ObjectId,
    at_seconds: i64,
) -> Result<Option<Member>> {
    let mut cursor = Some(tip);
    while let Some(oid) = cursor {
        let commit = expect_commit(objects, oid)?;
        if commit.committer_seconds <= at_seconds {
            let member: Member = facet_git_tree::deserialize(&commit.tree, objects)
                .map_err(|source| Error::Entity { oid, source })?;
            return Ok(Some(member));
        }
        cursor = commit.parents.first().copied();
    }
    Ok(None)
}
