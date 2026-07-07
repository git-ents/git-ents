//! `reachability-maintenance`: the effect that (re)generates a repo's
//! commit-graph and reachable-set artifacts (`docs/scale-out.adoc`,
//! "Reachability": "Maintenance effects generate commit-graph and
//! reachability bitmaps", "Regeneration is scheduled with repack (WS9) and
//! triggered by ref-update volume thresholds").
//!
//! Follows `git-effect`'s definition/execution split at the seam that
//! already exists for it: [`git_backend::EffectDef`] is the same static
//! "what to spawn" shape `git_effect::Effect` mirrors for user-configured
//! push effects (`refs/meta/effects/*`); [`definition`] returns one for this
//! effect. Unlike those, `reachability-maintenance` is not user-configured
//! or pushable — there is no `refs/meta/effects/reachability-maintenance`
//! ref, and [`regenerate`] is plain in-process maintenance code, not a
//! sandboxed shell command, so [`git_backend::EffectDef::command`] is
//! `None`.
//!
//! Scheduling — deciding *when* an `EffectExecutor` (WS7) actually spawns
//! this effect, e.g. alongside repack or on a timer — is WS9's job. This
//! module only supplies the trigger predicate ([`should_regenerate`]) and
//! the effect body ([`regenerate`]) a future scheduler calls.

use git_backend::{EffectDef, ObjectStore, RefStore};
use odb_tigris::registry::{ArtifactKind, PackRegistry};
use odb_tigris::transport::BlobTransport;

use crate::commitgraph::CommitGraph;
use crate::reachable_set::ReachableSetArtifact;
use crate::walk::StoreSource;
use crate::{Result, ref_tips, store};

/// The name this effect is identified by wherever an [`EffectDef`] needs
/// one — distinct from any `refs/meta/effects/*` name, which names a
/// user-configured push effect instead.
pub const EFFECT_NAME: &str = "reachability-maintenance";

/// The static [`EffectDef`] an `EffectExecutor` (WS7) spawns to run
/// [`regenerate`]. `command` and `image` are `None`: this effect runs as
/// in-process maintenance code, never a sandboxed shell command — those
/// fields exist on [`EffectDef`] for the general case, not because this
/// effect needs them.
#[must_use]
pub fn definition() -> EffectDef {
    EffectDef {
        name: EFFECT_NAME.to_owned(),
        command: None,
        image: None,
    }
}

/// Whether enough ref-update volume has accumulated since the last
/// regeneration to warrant running this effect again — the trigger
/// `docs/scale-out.adoc` calls for ("triggered by ref-update volume
/// thresholds"). Pure and total: a scheduler (WS9) is responsible for
/// tracking `ref_updates_since_last` and deciding when to actually spawn
/// the effect; this function only answers the yes/no question.
#[must_use]
pub fn should_regenerate(ref_updates_since_last: u64, threshold: u64) -> bool {
    ref_updates_since_last >= threshold
}

/// Regenerate `repo_id`'s commit-graph and reachable-set artifacts from
/// `refs`'s current tips over `objects`, storing both via `transport`/
/// `registry` (`docs/scale-out.adoc`, "Reachability"). Replaces whatever was
/// previously registered for each kind (`PackRegistry::record_artifact`) —
/// regeneration is a full recompute, not an incremental update.
///
/// # Errors
///
/// Returns an error if the ref store or object store cannot be read, if
/// building either artifact fails, or if storing either fails.
pub fn regenerate(
    repo_id: &str,
    refs: &dyn RefStore,
    objects: &dyn ObjectStore,
    transport: &dyn BlobTransport,
    registry: &dyn PackRegistry,
) -> Result<()> {
    let tips = ref_tips(refs)?;
    let source = StoreSource::new(objects);

    let graph = CommitGraph::build(tips.iter().copied(), &source)?;
    store::store_artifact(
        transport,
        registry,
        repo_id,
        ArtifactKind::CommitGraph,
        graph.serialize(),
    )?;

    let reachable = ReachableSetArtifact::build(tips, &source)?;
    store::store_artifact(
        transport,
        registry,
        repo_id,
        ArtifactKind::ReachableSet,
        reachable.serialize(),
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "unit test")]

    use odb_tigris::registry::memory::InMemoryRegistry;
    use odb_tigris::transport::fs::FsTransport;

    use super::*;

    #[test]
    fn should_regenerate_trips_at_the_threshold() {
        assert!(!should_regenerate(4, 5));
        assert!(should_regenerate(5, 5));
        assert!(should_regenerate(6, 5));
    }

    #[test]
    fn definition_names_the_effect_with_no_shell_command() {
        let def = definition();
        assert_eq!(def.name, EFFECT_NAME);
        assert!(def.command.is_none());
        assert!(def.image.is_none());
    }

    #[test]
    fn regenerate_stores_both_artifacts_from_a_real_repo() {
        let bare = tempfile::tempdir().unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "-q", "--bare", "-b", "main"])
            .arg(bare.path())
            .status()
            .unwrap();
        assert!(status.success());

        let work = tempfile::tempdir().unwrap();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.email", "test@example.com"],
            &["config", "user.name", "test"],
        ] {
            assert!(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(work.path())
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        std::fs::write(work.path().join("file"), "content").unwrap();
        for args in [
            &["add", "-A"][..],
            &["commit", "-q", "-m", "commit"],
            &[
                "push",
                bare.path().to_str().unwrap(),
                "main:refs/heads/main",
            ],
        ] {
            assert!(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(work.path())
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        }

        let refs = refstore_files::FilesRefStore::open(bare.path()).unwrap();
        let objects = odb_files::OdbFiles::open(bare.path()).unwrap();
        let artifact_dir = tempfile::tempdir().unwrap();
        let transport = FsTransport::open(artifact_dir.path()).unwrap();
        let registry = InMemoryRegistry::new();

        regenerate("repo", &refs, &objects, &transport, &registry).unwrap();

        let bundle = store::load_bundle(&transport, &registry, "repo").unwrap();
        assert!(bundle.commit_graph.is_some());
        assert!(bundle.reachable_set.is_some());
        let reachable = bundle.reachable_set.unwrap();
        // commit + tree + blob.
        assert_eq!(reachable.objects.len(), 3);
    }
}
