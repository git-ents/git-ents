//! [`Negotiate`] on the storage traits: the wants/haves reduction is the
//! same reachability walk ([`crate::walk`]) negotiation, push connectivity
//! checking, and GC mark all share (`docs/scale-out.adoc`, "Reachability").
//!
//! Routed through [`gix_reachability::engine::accelerated_reachable`]
//! (WS6): a commit-graph and, whenever a client's `haves` happen to equal a
//! server-known tip-frontier, a cached reachable-set snapshot both
//! accelerate this — absent either artifact, it is exactly the
//! correctness-first, one-object-at-a-time walk it always was.

use gix_reachability::engine::accelerated_reachable;

use super::{BackendResolver, NativeBackend};
use crate::types::{NegotiationState, PackPlan};
use crate::walk::StoreSource;
use crate::{Negotiate, Result};

impl<R: BackendResolver> Negotiate for NativeBackend<R> {
    fn wants_haves(&self, session: &mut NegotiationState) -> Result<PackPlan> {
        let backends = self.backends(&session.repo)?;
        let source = StoreSource::new(backends.objects.as_ref());

        // The client's claimed haves define the boundary negotiation must
        // not resend anything behind — tolerate a have the server never
        // actually had (a stale or misremembered claim) rather than fail
        // the whole negotiation over it.
        let haves_closure = accelerated_reachable(
            session.haves.iter().copied(),
            &source,
            |_id| false,
            true,
            &backends.reachability,
        )?;

        // Everything reachable from `wants`, not descending past the haves
        // boundary. A want neither the haves boundary nor the store itself
        // can resolve is a real negotiation failure, so this walk is
        // strict.
        let wants_seen = accelerated_reachable(
            session.wants.iter().copied(),
            &source,
            |id| haves_closure.contains(id),
            false,
            &backends.reachability,
        )?;

        let objects = wants_seen.difference(&haves_closure).copied().collect();
        Ok(PackPlan {
            repo: session.repo.clone(),
            objects,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "test fixture")]

    use super::*;
    use crate::native::NativeBackend;
    use crate::native::test_support::{FixedResolver, bare_repo, commit_onto, test_signer};

    #[test]
    fn plans_every_object_reachable_from_wants_when_haves_is_empty() {
        let bare = bare_repo();
        let commit = commit_onto(bare.path(), "file", "content");
        let (_key_dir, signer) = test_signer();
        let backend = NativeBackend::new(FixedResolver::open(bare.path()), signer);

        let mut session = NegotiationState {
            repo: crate::RepoId::new("repo"),
            wants: vec![commit],
            haves: Vec::new(),
        };
        let plan = backend.wants_haves(&mut session).unwrap();
        // commit + tree + blob.
        assert_eq!(plan.objects.len(), 3);
        assert!(plan.objects.contains(&commit));
    }

    #[test]
    fn plans_nothing_when_haves_already_covers_wants() {
        let bare = bare_repo();
        let commit = commit_onto(bare.path(), "file", "content");
        let (_key_dir, signer) = test_signer();
        let backend = NativeBackend::new(FixedResolver::open(bare.path()), signer);

        let mut session = NegotiationState {
            repo: crate::RepoId::new("repo"),
            wants: vec![commit],
            haves: vec![commit],
        };
        let plan = backend.wants_haves(&mut session).unwrap();
        assert!(plan.objects.is_empty());
    }
}
