//! [`accelerated_reachable`]: the entry point negotiation, ingest
//! connectivity, and GC mark all call instead of [`crate::walk::reachable`]
//! directly (`docs/scale-out.adoc`, "Reachability").
//!
//! Two independent accelerations, either or both possibly absent:
//!
//! - An exact tip-frontier match against a cached
//!   [`crate::reachable_set::ReachableSetArtifact`] answers instantly, no
//!   walk at all — see that module's docs for why exact match is the right
//!   bar rather than a more general (and more expensive to verify) ancestor
//!   check.
//! - Otherwise, [`crate::walk::reachable_with_graph`] still benefits from a
//!   [`crate::commitgraph::CommitGraph`] wherever it covers the walk's
//!   commits, and degrades to a plain [`crate::walk::reachable`] wherever it
//!   doesn't.

use std::collections::BTreeSet;

use gix_hash::ObjectId;

use crate::Result;
use crate::commitgraph::CommitGraph;
use crate::reachable_set::ReachableSetArtifact;
use crate::walk::{self, ObjectSource};

/// The reachability artifacts currently available for one repository —
/// possibly neither, in which case [`accelerated_reachable`] is exactly
/// [`crate::walk::reachable`] (`docs/scale-out.adoc`'s "absence ...
/// degrades speed, never answers").
#[derive(Debug, Clone, Default)]
pub struct ArtifactBundle {
    /// The commit-parent accelerator, if generated.
    pub commit_graph: Option<CommitGraph>,
    /// The tip-frontier reachable-set snapshot, if generated.
    pub reachable_set: Option<ReachableSetArtifact>,
}

impl ArtifactBundle {
    /// No artifacts at all — every consumer using this degrades fully to
    /// the slow walk. The default for a repo whose maintenance effect has
    /// never run, and for any backend that hasn't wired artifact loading
    /// yet.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }
}

/// [`crate::walk::reachable`], accelerated by whatever `artifacts` holds.
///
/// # Errors
///
/// Returns an error under the same conditions as
/// [`crate::walk::reachable_with_graph`].
pub fn accelerated_reachable(
    roots: impl IntoIterator<Item = ObjectId>,
    source: &dyn ObjectSource,
    stop: impl FnMut(&ObjectId) -> bool,
    lenient: bool,
    artifacts: &ArtifactBundle,
) -> Result<BTreeSet<ObjectId>> {
    let roots: Vec<ObjectId> = roots.into_iter().collect();

    if let Some(set) = &artifacts.reachable_set {
        let root_set: BTreeSet<ObjectId> = roots.iter().copied().collect();
        if set.frontier == root_set {
            return Ok(set.objects.clone());
        }
    }

    walk::reachable_with_graph(
        roots,
        source,
        stop,
        lenient,
        artifacts.commit_graph.as_ref(),
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "unit test")]

    use std::collections::HashMap;

    use gix_object::Kind;

    use super::*;

    struct FakeBlobs(HashMap<ObjectId, (Kind, Vec<u8>)>);

    impl ObjectSource for FakeBlobs {
        fn find(&self, id: &ObjectId) -> Result<Option<(Kind, Vec<u8>)>> {
            Ok(self.0.get(id).cloned())
        }
    }

    fn oid(byte: u8) -> ObjectId {
        let mut bytes = [0_u8; 20];
        if let Some(last) = bytes.last_mut() {
            *last = byte;
        }
        ObjectId::from(bytes)
    }

    #[test]
    fn exact_frontier_match_short_circuits_the_walk() {
        let blob = oid(1);
        // No entry for `blob` in the source at all: if the fast path didn't
        // fire, this would error rather than return the cached set.
        let source = FakeBlobs(HashMap::new());
        let cached = ReachableSetArtifact {
            frontier: BTreeSet::from([blob]),
            objects: BTreeSet::from([blob]),
        };
        let artifacts = ArtifactBundle {
            commit_graph: None,
            reachable_set: Some(cached),
        };

        let result =
            accelerated_reachable([blob], &source, |_id| false, false, &artifacts).unwrap();
        assert_eq!(result, BTreeSet::from([blob]));
    }

    #[test]
    fn a_frontier_mismatch_falls_back_to_a_real_walk() {
        let blob = oid(1);
        let other = oid(2);
        let mut objects = HashMap::new();
        objects.insert(other, (Kind::Blob, b"content".to_vec()));
        let source = FakeBlobs(objects);
        let cached = ReachableSetArtifact {
            frontier: BTreeSet::from([blob]),
            objects: BTreeSet::from([blob]),
        };
        let artifacts = ArtifactBundle {
            commit_graph: None,
            reachable_set: Some(cached),
        };

        let result =
            accelerated_reachable([other], &source, |_id| false, false, &artifacts).unwrap();
        assert_eq!(result, BTreeSet::from([other]));
    }

    #[test]
    fn no_artifacts_at_all_behaves_like_the_plain_walk() {
        let blob = oid(1);
        let mut objects = HashMap::new();
        objects.insert(blob, (Kind::Blob, b"content".to_vec()));
        let source = FakeBlobs(objects);

        let result = accelerated_reachable(
            [blob],
            &source,
            |_id| false,
            false,
            &ArtifactBundle::empty(),
        )
        .unwrap();
        assert_eq!(result, BTreeSet::from([blob]));
    }
}
