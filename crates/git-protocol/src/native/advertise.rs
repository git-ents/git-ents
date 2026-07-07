//! [`Advertise`] on the storage traits: every ref [`AdSpec`] selects, read
//! straight from [`git_backend::RefStore::iter_prefix`].

use git_backend::RefName;

use super::{BackendResolver, NativeBackend};
use crate::types::{AdSpec, RefAdvertisement};
use crate::{Advertise, Result};

impl<R: BackendResolver> Advertise for NativeBackend<R> {
    fn refs(&self, repo: &crate::RepoId, filter: &AdSpec) -> Result<RefAdvertisement> {
        let backends = self.backends(repo)?;
        let refs: Vec<(RefName, gix_hash::ObjectId)> = backends
            .refs
            .iter_prefix(&filter.prefix)?
            .collect::<git_backend::Result<_>>()?;

        // `HEAD` itself is never under a `refs/` prefix, so it's resolved
        // separately and matched back against the advertised refs by tip —
        // an approximation (a detached `HEAD` whose tip happens to equal a
        // branch's is indistinguishable from that branch) that is good
        // enough for the smart-HTTP default-branch hint `git clone` uses.
        // An unborn `HEAD` (a fresh repository with no commits yet) fails
        // to resolve at all; treated the same as `HEAD` naming nothing.
        let head_tip = backends.refs.get(&RefName::new("HEAD")).ok().flatten();
        let head = head_tip.and_then(|tip| {
            refs.iter()
                .find(|(_, oid)| *oid == tip)
                .map(|(name, _)| name.clone())
        });

        Ok(RefAdvertisement { refs, head })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "test fixture")]

    use super::*;
    use crate::native::NativeBackend;
    use crate::native::test_support::{FixedResolver, bare_repo, commit_onto, test_signer};

    #[test]
    fn advertises_every_ref_under_the_prefix_and_resolves_head() {
        let bare = bare_repo();
        let commit = commit_onto(bare.path(), "file", "content");
        let (_key_dir, signer) = test_signer();

        let backend = NativeBackend::new(FixedResolver::open(bare.path()), signer);
        let ad = backend
            .refs(&crate::RepoId::new("repo"), &AdSpec::everything())
            .unwrap();
        assert!(
            ad.refs
                .iter()
                .any(|(name, oid)| name.as_str() == "refs/heads/main" && *oid == commit)
        );
        assert_eq!(ad.head, Some(RefName::new("refs/heads/main")));
    }
}
