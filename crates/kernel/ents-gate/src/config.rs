//! The verification epoch, read from `refs/meta/config` (`gate.epoch`).

use facet::Facet;
use gix_hash::ObjectId;
use gix_object::Find;
use gix_ref_store::RefStoreRead;

use crate::error::{Error, Result};
use crate::object::expect_commit;

/// The slice of `refs/meta/config`'s typed tree the gate consults: the
/// verification epoch (`gate.epoch`).
///
/// `model.sdoc` defines no Config entity yet, so this struct is the
/// first (and currently only) definition of the config tree's shape; it
/// lives here rather than in `ents-model` because the epoch is the only
/// field any crate reads today. When configuration grows non-gate fields
/// (description, role rules, ...), the entity moves to `ents-model` and
/// that change is a storage migration like any other struct change
/// (`meta-ref.migration`).
///
/// `epoch` is `None` on a config written before verification was turned
/// on. Once it is `Some`, the gate applies the tip invariant to every
/// `refs/meta/*` update; the value records *when* (seconds since the
/// Unix epoch) enforcement began, for audit tooling — the live gate only
/// tests presence, because every proposed update is by definition after
/// the epoch that admits it.
///
/// # Examples
///
/// ```
/// use ents_gate::Config;
///
/// let config = Config { epoch: Some(1_700_000_000) };
/// let (root, store) = facet_git_tree::serialize(&config).expect("serialize");
/// let back: Config = facet_git_tree::deserialize(&root, &store).expect("deserialize");
/// assert_eq!(back, config);
/// ```
// @relation(gate.epoch, scope=file)
#[derive(Debug, Clone, Default, PartialEq, Eq, Facet)]
pub struct Config {
    /// When the tip invariant came into force, seconds since the Unix
    /// epoch; `None` while verification has never been enabled.
    pub epoch: Option<u64>,
}

/// The epoch recorded by the config tree of the commit at `oid`, or an
/// [`Error::Entity`] when the tree does not parse as [`Config`] — an
/// unreadable config fails closed rather than silently disabling the
/// gate.
pub(crate) fn epoch_at_commit(objects: &dyn Find, oid: ObjectId) -> Result<Option<u64>> {
    let commit = expect_commit(objects, oid)?;
    let config: Config = facet_git_tree::deserialize(&commit.tree, objects)
        .map_err(|source| Error::Entity { oid, source })?;
    Ok(config.epoch)
}

/// The epoch currently in force, read from `refs/meta/config`'s tip;
/// `None` when the config ref does not exist or records no epoch.
// @relation(gate.epoch, gate.policy-as-state, scope=function)
pub(crate) fn current_epoch(refs: &dyn RefStoreRead, objects: &dyn Find) -> Result<Option<u64>> {
    #[expect(
        clippy::expect_used,
        clippy::unwrap_in_result,
        reason = "CONFIG_REF is a compile-time constant; the doctest below and \
                  ents-model's own tests pin its validity"
    )]
    let name: gix::refs::FullName = ents_model::namespace::CONFIG_REF
        .try_into()
        .expect("CONFIG_REF is a valid refname");
    match refs.get(name.as_ref())? {
        Some(tip) => epoch_at_commit(objects, tip),
        None => Ok(None),
    }
}
