//! `exec-local`: [`git_backend::EffectExecutor`] over this crate's existing
//! Docker-sandboxed local backend — the `exec-local` row of
//! `docs/scale-out.adoc`'s "EffectExecutor" table (WS7).
//!
//! A thin adapter, not a second execution path: the tree checkout goes
//! through [`local::sync_tree`], toolchains through the same
//! [`git_toolchain::export`] call every local backend uses (via
//! [`local::export_toolchains`]), caches through [`cache::restore_local`] /
//! [`cache::snapshot_local`], and the run itself through the engine's own
//! Docker backend. That is correctness rule 6 ("materialization is one code
//! path") applied to the local/remote split: `exec-local` and a
//! push-triggered engine run may differ in who orchestrates them, never in
//! the code that materializes and runs an effect.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex, PoisonError};
use std::thread::JoinHandle;

use git_backend::{
    EffectDef, EffectExecutor, EffectHandle, EffectStatus, Error, MaterializedInputs,
};

use crate::results::Status;
use crate::{cache, docker, engine, local};

/// [`EffectExecutor`] running each effect in a throwaway local Docker
/// container (see [`crate::docker`]), materialized from one repository.
/// `spawn` prepares the sandbox and hands the run to a worker thread;
/// `wait` joins it.
///
/// ## Requirements
///
/// @relation(checks.sandbox)
pub struct LocalExecutor {
    repo: PathBuf,
    running: StdMutex<HashMap<String, JoinHandle<Status>>>,
}

impl LocalExecutor {
    /// An executor materializing effects from (and snapshotting caches back
    /// to) `repo`.
    #[must_use]
    pub fn new(repo: impl Into<PathBuf>) -> Self {
        Self {
            repo: repo.into(),
            running: StdMutex::new(HashMap::new()),
        }
    }
}

/// Lock `mutex`, recovering the guard from a poisoned lock rather than
/// panicking — same rationale as `engine::lock`: a lost worker entry is
/// worth an error result, never a torn-down process.
fn lock<T>(mutex: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl EffectExecutor for LocalExecutor {
    fn spawn(
        &self,
        effect: &EffectDef,
        inputs: MaterializedInputs,
    ) -> git_backend::Result<EffectHandle> {
        let Some(command) = effect.command.clone() else {
            return Err(Error::Effect(format!(
                "effect {} is composite (no command); its outcome derives from its \
                 dependencies instead of a spawn",
                effect.name
            )));
        };
        docker::ensure_docker().map_err(Error::Effect)?;
        let sandbox = local::Sandbox::new().map_err(Error::Effect)?;
        local::sync_tree(&self.repo, &sandbox, inputs.tree).map_err(Error::Effect)?;

        // The map's keys name the toolchains to materialize; the PATH
        // entries activated in-container are derived from the Docker
        // backend's own bind-mount layout, exactly as
        // `engine::Backend::resolve_toolchains` derives them per backend —
        // a caller-resolved entry describes some other context's
        // filesystem, which this container never sees.
        let names: Vec<String> = inputs.toolchain_paths.keys().cloned().collect();
        local::export_toolchains(&self.repo, &sandbox, &names).map_err(Error::Effect)?;
        let dirs: HashMap<String, String> = names
            .iter()
            .map(|name| {
                (
                    name.clone(),
                    format!("{}/{name}/bin", docker::TOOLCHAINS_DIR),
                )
            })
            .collect();
        let mut command = engine::activate(&command, &names, &dirs);

        if let Some(name) = &inputs.cache {
            cache::restore_local(&self.repo, &sandbox.cache_dir(name), name)
                .map_err(Error::Effect)?;
            command =
                engine::with_cache_env(&command, Some(&format!("{}/{name}", docker::CACHE_DIR)));
        }

        let id = uuid::Uuid::new_v4().to_string();
        let repo = self.repo.clone();
        let name = effect.name.clone();
        let cache_name = inputs.cache.clone();
        let worker = std::thread::spawn(move || {
            let live = Arc::new(StdMutex::new(String::new()));
            let result = engine::run_one_docker(&sandbox, &name, &command, &live);
            if let Some(cache_name) = cache_name
                && let Err(e) =
                    cache::snapshot_local(&repo, &sandbox.cache_dir(&cache_name), &cache_name)
            {
                eprintln!("effects: could not snapshot cache {cache_name}: {e}");
            }
            result.status
        });
        lock(&self.running).insert(id.clone(), worker);
        Ok(EffectHandle { id })
    }

    fn wait(&self, handle: &EffectHandle) -> git_backend::Result<EffectStatus> {
        let Some(worker) = lock(&self.running).remove(&handle.id) else {
            return Err(Error::Effect(format!(
                "unknown effect handle {}",
                handle.id
            )));
        };
        let status = match worker.join() {
            Ok(status) => status,
            Err(_panic) => {
                return Err(Error::Effect(
                    "the effect's worker thread panicked".to_owned(),
                ));
            }
        };
        Ok(match status {
            Status::Pass => EffectStatus::Pass,
            Status::Fail => EffectStatus::Fail,
            // `run_one_docker` only settles Pass/Fail/Error; the queued /
            // running / skipped states are engine bookkeeping it never
            // returns.
            _ => EffectStatus::Error,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::assertions_on_result_states,
        reason = "unit test"
    )]

    use std::collections::BTreeMap;
    use std::process::Command;

    use gix_hash::ObjectId;

    use super::*;

    fn inputs(tree: ObjectId) -> MaterializedInputs {
        MaterializedInputs {
            tree,
            toolchain_paths: BTreeMap::new(),
            cache: None,
        }
    }

    fn zero_tree() -> ObjectId {
        ObjectId::from_hex(b"0000000000000000000000000000000000000000").unwrap()
    }

    // @relation(checks.sandbox, role=Verifies)
    #[test]
    fn a_composite_effect_is_never_spawned() {
        let executor = LocalExecutor::new("/nonexistent");
        let effect = EffectDef {
            name: "all".to_owned(),
            command: None,
            image: None,
        };
        assert!(executor.spawn(&effect, inputs(zero_tree())).is_err());
    }

    // @relation(checks.sandbox, role=Verifies)
    #[test]
    fn waiting_on_an_unknown_handle_is_an_error() {
        let executor = LocalExecutor::new("/nonexistent");
        let handle = EffectHandle {
            id: "no-such-run".to_owned(),
        };
        assert!(executor.wait(&handle).is_err());
    }

    // @relation(checks.sandbox, role=Verifies)
    #[test]
    #[cfg_attr(
        windows,
        ignore = "windows runners use Windows containers; no Linux image support"
    )]
    fn local_executor_runs_a_trivial_effect() {
        if docker::ensure_docker().is_err() {
            eprintln!("skipping local_executor_runs_a_trivial_effect: docker is not available");
            return;
        }

        let repo = crate::testutil::unique_repo("exec-local");
        let status = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["commit", "--allow-empty", "-q", "-m", "seed"])
            .status()
            .unwrap();
        assert!(status.success());
        let tree = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["rev-parse", "HEAD^{tree}"])
            .output()
            .unwrap();
        assert!(tree.status.success());
        let tree =
            ObjectId::from_hex(String::from_utf8(tree.stdout).unwrap().trim().as_bytes()).unwrap();

        let executor = LocalExecutor::new(&repo);
        let effect = EffectDef {
            name: "hello".to_owned(),
            command: Some("echo hi-from-exec-local".to_owned()),
            image: None,
        };
        let handle = executor.spawn(&effect, inputs(tree)).unwrap();
        assert_eq!(executor.wait(&handle).unwrap(), EffectStatus::Pass);
        // The handle's backend-side state is consumed by the first wait.
        assert!(executor.wait(&handle).is_err());
    }
}
