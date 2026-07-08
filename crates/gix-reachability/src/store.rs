//! Persisting reachability artifacts beside packs, tracked in the pack
//! registry (`docs/scale-out.adoc`, "Reachability": "stored beside packs,
//! tracked in the pack registry") — the same `BlobTransport` +
//! `PackRegistry` seam `odb-tigris` uses for packs themselves, reused here
//! rather than inventing a parallel storage path.

use odb_tigris::registry::{ArtifactKind, ArtifactRecord, PackRegistry};
use odb_tigris::transport::BlobTransport;

use crate::commitgraph::CommitGraph;
use crate::engine::ArtifactBundle;
use crate::reachable_set::ReachableSetArtifact;
use crate::{Error, Result};

fn artifact_key(repo_id: &str, kind: ArtifactKind) -> String {
    format!("{repo_id}/reachability/{}.bin", kind.as_str())
}

/// Write `bytes` as `repo_id`'s current artifact of `kind`, replacing
/// whatever was previously registered.
///
/// # Errors
///
/// Returns an error if the transport write or the registry record fails.
pub fn store_artifact(
    transport: &dyn BlobTransport,
    registry: &dyn PackRegistry,
    repo_id: &str,
    kind: ArtifactKind,
    bytes: Vec<u8>,
) -> Result<()> {
    let key = artifact_key(repo_id, kind);
    transport.put(&key, bytes).map_err(Error::Backend)?;
    registry
        .record_artifact(ArtifactRecord {
            repo_id: repo_id.to_owned(),
            kind,
            key,
        })
        .map_err(Error::Backend)
}

/// Load `repo_id`'s current artifact bytes of `kind`, or `None` if it has
/// never been generated.
///
/// # Errors
///
/// Returns an error if the registry or transport read fails.
pub fn load_artifact(
    transport: &dyn BlobTransport,
    registry: &dyn PackRegistry,
    repo_id: &str,
    kind: ArtifactKind,
) -> Result<Option<Vec<u8>>> {
    let Some(record) = registry
        .get_artifact(repo_id, kind)
        .map_err(Error::Backend)?
    else {
        return Ok(None);
    };
    let bytes = transport.get(&record.key).map_err(Error::Backend)?;
    Ok(Some(bytes))
}

/// Remove `repo_id`'s artifact of `kind`, both its bucket bytes and its
/// registry record. Not an error if already absent.
///
/// # Errors
///
/// Returns an error if the transport or registry delete fails.
pub fn delete_artifact(
    transport: &dyn BlobTransport,
    registry: &dyn PackRegistry,
    repo_id: &str,
    kind: ArtifactKind,
) -> Result<()> {
    if let Some(record) = registry
        .get_artifact(repo_id, kind)
        .map_err(Error::Backend)?
    {
        transport.delete(&record.key).map_err(Error::Backend)?;
    }
    registry
        .delete_artifact(repo_id, kind)
        .map_err(Error::Backend)
}

/// Load and parse `repo_id`'s full [`ArtifactBundle`], degrading each
/// artifact independently to `None` when absent — never an error just
/// because one or both artifacts don't exist yet.
///
/// # Errors
///
/// Returns an error only if a *present* artifact fails to parse (corrupt,
/// or from an incompatible format version) — never for a missing one.
pub fn load_bundle(
    transport: &dyn BlobTransport,
    registry: &dyn PackRegistry,
    repo_id: &str,
) -> Result<ArtifactBundle> {
    let commit_graph = load_artifact(transport, registry, repo_id, ArtifactKind::CommitGraph)?
        .map(|bytes| CommitGraph::deserialize(&bytes))
        .transpose()?;
    let reachable_set = load_artifact(transport, registry, repo_id, ArtifactKind::ReachableSet)?
        .map(|bytes| ReachableSetArtifact::deserialize(&bytes))
        .transpose()?;
    Ok(ArtifactBundle {
        commit_graph,
        reachable_set,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "unit test")]

    use odb_tigris::registry::memory::InMemoryRegistry;
    use odb_tigris::transport::fs::FsTransport;

    use super::*;

    #[test]
    fn store_then_load_bundle_round_trips_both_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let transport = FsTransport::open(dir.path()).unwrap();
        let registry = InMemoryRegistry::new();

        let graph = CommitGraph::default();
        store_artifact(
            &transport,
            &registry,
            "repo",
            ArtifactKind::CommitGraph,
            graph.serialize(),
        )
        .unwrap();

        let bundle = load_bundle(&transport, &registry, "repo").unwrap();
        assert!(bundle.commit_graph.is_some());
        assert!(bundle.reachable_set.is_none());
    }

    #[test]
    fn load_bundle_is_empty_when_nothing_was_ever_generated() {
        let dir = tempfile::tempdir().unwrap();
        let transport = FsTransport::open(dir.path()).unwrap();
        let registry = InMemoryRegistry::new();

        let bundle = load_bundle(&transport, &registry, "repo").unwrap();
        assert!(bundle.commit_graph.is_none());
        assert!(bundle.reachable_set.is_none());
    }

    #[test]
    fn delete_artifact_removes_it_from_both_transport_and_registry() {
        let dir = tempfile::tempdir().unwrap();
        let transport = FsTransport::open(dir.path()).unwrap();
        let registry = InMemoryRegistry::new();

        let graph = CommitGraph::default();
        store_artifact(
            &transport,
            &registry,
            "repo",
            ArtifactKind::CommitGraph,
            graph.serialize(),
        )
        .unwrap();
        delete_artifact(&transport, &registry, "repo", ArtifactKind::CommitGraph).unwrap();

        assert!(
            load_artifact(&transport, &registry, "repo", ArtifactKind::CommitGraph)
                .unwrap()
                .is_none()
        );
    }
}
