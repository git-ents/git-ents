//! `exec-sprites`: [`git_backend::EffectExecutor`] over Fly Machines
//! ("Sprites") — the hosted row of `docs/scale-out.adoc`'s "EffectExecutor"
//! table (WS7: the dispatcher drains the effect queue and creates Sprites
//! through this crate).
//!
//! One machine per effect, created through a [`SpriteLauncher`].
//! [`FlyLauncher`] is the real one: it shells out to the `fly` (flyctl)
//! CLI, the same way the workspace's other sandbox backends shell out to
//! `docker` and `sprite` — no HTTP client dependency, per the dependency
//! policy.
//!
//! # What comes back, and how
//!
//! Nothing returns through the machine. Results and cache entries return
//! via *attested push* signed with the worker member key the machine is
//! provisioned with (`docs/scale-out.adoc`, WS7 and "Attested push": key
//! availability in Sprites is exactly the enrollment cost uniform-strong
//! attestation already pays). [`WorkerKey`] models that provisioning: the
//! machine spec carries only the *name* of the secret-provisioned
//! environment variable holding the key material — the material itself is
//! set out-of-band (`fly secrets set`, or machine secrets at deploy time)
//! and never travels through a machine-create argument, where
//! `fly machine status` would echo it. The launcher consequently observes
//! only machine lifecycle; [`git_backend::EffectExecutor::wait`] here
//! settles [`EffectStatus::SettledRemotely`], and the recorded run refs are
//! the outcome's source of truth.
//!
//! # The image (WS8)
//!
//! [`SpriteConfig::image`] (or an effect's own `image` override) is
//! expected to carry a baked toolchain object store (WS8, "Hydration and
//! toolchains"): materialization inside the machine stays the one code
//! path of correctness rule 6, the baked store merely being the tier that
//! answers `read` on a hit, with a miss falling through to fetch. Nothing
//! here bakes or verifies images; this crate only names what to boot.
//!
//! # What needs a real deployment
//!
//! Everything assembled here — machine specs, argv, env plumbing — is pure
//! and unit-tested against a fake launcher. What is *not* claimable
//! in-repo: flyctl's output shapes ([`fly::parse_machine_id`], the status
//! text [`FlyLauncher`] polls) and the end-to-end attested results push,
//! which need a deployed Fly app and a provisioned worker member key to
//! exercise. Those spots carry their own deploy-only notes.

mod fly;

pub use fly::FlyLauncher;

use std::collections::BTreeMap;

use git_backend::{
    EffectDef, EffectExecutor, EffectHandle, EffectStatus, Error, MaterializedInputs, Result,
};

/// Env var carrying the effect's name into the machine.
pub const EFFECT_ENV: &str = "GIT_ENTS_EFFECT";

/// Env var carrying the tree the effect runs against (full hex OID).
pub const TREE_ENV: &str = "GIT_ENTS_TREE";

/// Env var carrying the remote the in-machine runner pushes its results
/// and cache refs to (the attested push's destination).
pub const RESULTS_REMOTE_ENV: &str = "GIT_ENTS_RESULTS_REMOTE";

/// Env var carrying the worker member name whose key signs the results
/// push.
pub const WORKER_MEMBER_ENV: &str = "GIT_ENTS_WORKER_MEMBER";

/// Env var carrying the *name* of the secret-provisioned env var that
/// holds the worker member's private key material (see [`WorkerKey`]).
pub const WORKER_KEY_ENV: &str = "GIT_ENTS_WORKER_KEY_ENV";

/// Env var carrying the colon-joined `PATH` entries of the activated
/// toolchains, in toolchain-name order.
pub const TOOLCHAIN_PATH_ENV: &str = "GIT_ENTS_TOOLCHAIN_PATH";

/// Env var carrying the effect's cache name, when it declares one.
pub const CACHE_ENV: &str = "GIT_ENTS_CACHE";

/// Everything a launcher needs to create one machine: which image to boot,
/// what to run in it, and the environment the in-machine runner reads its
/// work order from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineSpec {
    /// The machine's name (derived from the effect and its tree).
    pub name: String,
    /// The image to boot — expected to carry the baked toolchain object
    /// store (WS8).
    pub image: String,
    /// The environment the runner reads its work order from. Never carries
    /// key material, only the name of the secret that does.
    pub env: BTreeMap<String, String>,
    /// The effect's shell command, run under `sh -c`.
    pub command: String,
}

/// How a Sprite actually gets created and reaped. [`FlyLauncher`] shells
/// out to flyctl; tests substitute a fake to assert the [`MachineSpec`]
/// without any Fly dependency.
pub trait SpriteLauncher: Send + Sync {
    /// Create and start a machine per `spec`, returning its backend id.
    /// Must not block for the effect's completion.
    fn launch(&self, spec: &MachineSpec) -> Result<String>;

    /// Block until machine `machine` has settled (stopped, or already
    /// reaped).
    fn wait(&self, machine: &str) -> Result<()>;
}

/// The worker member identity a machine pushes results back as: an
/// enrolled member (`refs/meta/members/*`) whose key material is
/// provisioned to the machine as a secret env var named
/// [`WorkerKey::key_env`]. Modeled as configuration because the material
/// itself must stay out of machine-create arguments; provisioning the
/// secret is a deploy step this crate cannot perform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerKey {
    /// The enrolled worker member's name.
    pub member: String,
    /// The name of the env var (a Fly secret) holding the member's private
    /// key material inside the machine.
    pub key_env: String,
}

/// The executor's fixed configuration: the default image, where results
/// push back to, and the worker member identity that signs the push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpriteConfig {
    /// The default image to boot when an effect names none — expected to
    /// carry the baked toolchain object store (WS8).
    pub image: String,
    /// The remote the in-machine runner pushes results and cache refs to.
    pub results_remote: String,
    /// The worker member identity signing that push.
    pub worker_key: WorkerKey,
}

/// [`EffectExecutor`] creating one machine per spawned effect through a
/// [`SpriteLauncher`].
pub struct SpriteExecutor<L> {
    launcher: L,
    config: SpriteConfig,
}

impl<L: SpriteLauncher> SpriteExecutor<L> {
    /// An executor creating machines through `launcher` per `config`.
    #[must_use]
    pub fn new(launcher: L, config: SpriteConfig) -> Self {
        Self { launcher, config }
    }
}

/// Assemble the [`MachineSpec`] for one effect — pure, so exactly what a
/// machine is created with (image selection, env plumbing, key-material
/// indirection) is unit-tested without a launcher.
///
/// # Errors
///
/// Returns [`Error::Effect`] for a composite effect (no command): the
/// engine derives its outcome from its dependencies instead of spawning it.
pub fn machine_spec(
    config: &SpriteConfig,
    effect: &EffectDef,
    inputs: &MaterializedInputs,
) -> Result<MachineSpec> {
    let Some(command) = effect.command.clone() else {
        return Err(Error::Effect(format!(
            "effect {} is composite (no command); its outcome derives from its \
             dependencies instead of a spawn",
            effect.name
        )));
    };

    let tree = inputs.tree.to_string();
    let mut env = BTreeMap::new();
    env.insert(EFFECT_ENV.to_owned(), effect.name.clone());
    env.insert(TREE_ENV.to_owned(), tree.clone());
    env.insert(RESULTS_REMOTE_ENV.to_owned(), config.results_remote.clone());
    env.insert(
        WORKER_MEMBER_ENV.to_owned(),
        config.worker_key.member.clone(),
    );
    env.insert(WORKER_KEY_ENV.to_owned(), config.worker_key.key_env.clone());
    if !inputs.toolchain_paths.is_empty() {
        let path = inputs
            .toolchain_paths
            .values()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(":");
        env.insert(TOOLCHAIN_PATH_ENV.to_owned(), path);
    }
    if let Some(cache) = &inputs.cache {
        env.insert(CACHE_ENV.to_owned(), cache.clone());
    }

    Ok(MachineSpec {
        name: machine_name(&effect.name, &tree),
        image: effect.image.clone().unwrap_or_else(|| config.image.clone()),
        env,
        command,
    })
}

/// A machine name for `effect` at `tree_hex`, kept to the `[a-z0-9-]` a
/// machine name allows (mirroring `git_effect::engine::sprite_name`'s
/// sanitization): `effect-<name>-<tree prefix>`.
fn machine_name(effect: &str, tree_hex: &str) -> String {
    let sanitized: String = effect
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = sanitized.trim_matches('-');
    let name = if trimmed.is_empty() {
        "effect"
    } else {
        trimmed
    };
    let short = tree_hex.get(..12).unwrap_or(tree_hex);
    format!("effect-{name}-{short}")
}

impl<L: SpriteLauncher> EffectExecutor for SpriteExecutor<L> {
    fn spawn(&self, effect: &EffectDef, inputs: MaterializedInputs) -> Result<EffectHandle> {
        let spec = machine_spec(&self.config, effect, &inputs)?;
        let id = self.launcher.launch(&spec)?;
        Ok(EffectHandle { id })
    }

    fn wait(&self, handle: &EffectHandle) -> Result<EffectStatus> {
        self.launcher.wait(&handle.id)?;
        // The machine's termination is all this executor can observe; the
        // outcome itself returns via the attested results push (crate
        // docs). Exit-code sniffing through flyctl is deliberately not
        // attempted — it would duplicate, and could contradict, the
        // recorded run refs.
        Ok(EffectStatus::SettledRemotely)
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::unwrap_in_result,
        clippy::indexing_slicing,
        clippy::assertions_on_result_states,
        reason = "unit test"
    )]

    use std::sync::Mutex;

    use super::*;

    fn tree() -> gix_hash::ObjectId {
        gix_hash::ObjectId::from_hex(b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap()
    }

    fn config() -> SpriteConfig {
        SpriteConfig {
            image: "registry.fly.io/git-ents-effects:baked".to_owned(),
            results_remote: "https://ents.example/repo.git".to_owned(),
            worker_key: WorkerKey {
                member: "worker-1".to_owned(),
                key_env: "WORKER_SSH_KEY".to_owned(),
            },
        }
    }

    fn effect(command: Option<&str>, image: Option<&str>) -> EffectDef {
        EffectDef {
            name: "Build & Test".to_owned(),
            command: command.map(str::to_owned),
            image: image.map(str::to_owned),
        }
    }

    fn inputs() -> MaterializedInputs {
        let mut toolchain_paths = BTreeMap::new();
        toolchain_paths.insert("rust".to_owned(), "/toolchains/aaa/bin".to_owned());
        toolchain_paths.insert("zig".to_owned(), "/toolchains/bbb/bin".to_owned());
        MaterializedInputs {
            tree: tree(),
            toolchain_paths,
            cache: Some("sccache".to_owned()),
        }
    }

    /// Captures every launched spec; `wait` records the machine id it was
    /// asked about.
    struct FakeLauncher {
        launched: Mutex<Vec<MachineSpec>>,
        waited: Mutex<Vec<String>>,
    }

    impl FakeLauncher {
        fn new() -> Self {
            Self {
                launched: Mutex::new(Vec::new()),
                waited: Mutex::new(Vec::new()),
            }
        }
    }

    impl SpriteLauncher for FakeLauncher {
        fn launch(&self, spec: &MachineSpec) -> Result<String> {
            self.launched.lock().unwrap().push(spec.clone());
            Ok(format!("machine-{}", self.launched.lock().unwrap().len()))
        }

        fn wait(&self, machine: &str) -> Result<()> {
            self.waited.lock().unwrap().push(machine.to_owned());
            Ok(())
        }
    }

    #[test]
    fn machine_spec_plumbs_image_env_and_key_material_indirection() {
        let spec = machine_spec(&config(), &effect(Some("cargo test"), None), &inputs()).unwrap();

        assert_eq!(spec.image, "registry.fly.io/git-ents-effects:baked");
        assert_eq!(spec.command, "cargo test");
        assert_eq!(spec.name, "effect-build---test-aaaaaaaaaaaa");
        assert_eq!(spec.env[EFFECT_ENV], "Build & Test");
        assert_eq!(spec.env[TREE_ENV], tree().to_string());
        assert_eq!(
            spec.env[RESULTS_REMOTE_ENV],
            "https://ents.example/repo.git"
        );
        assert_eq!(spec.env[WORKER_MEMBER_ENV], "worker-1");
        // Only the *name* of the secret-provisioned variable travels in
        // the spec — never key bytes.
        assert_eq!(spec.env[WORKER_KEY_ENV], "WORKER_SSH_KEY");
        assert_eq!(
            spec.env[TOOLCHAIN_PATH_ENV],
            "/toolchains/aaa/bin:/toolchains/bbb/bin"
        );
        assert_eq!(spec.env[CACHE_ENV], "sccache");
    }

    #[test]
    fn an_effects_own_image_overrides_the_default() {
        let spec = machine_spec(
            &config(),
            &effect(Some("true"), Some("registry.fly.io/custom:1")),
            &inputs(),
        )
        .unwrap();
        assert_eq!(spec.image, "registry.fly.io/custom:1");
    }

    #[test]
    fn a_composite_effect_is_never_launched() {
        assert!(machine_spec(&config(), &effect(None, None), &inputs()).is_err());
    }

    #[test]
    fn spawn_launches_and_wait_settles_remotely() {
        let executor = SpriteExecutor::new(FakeLauncher::new(), config());
        let handle = executor
            .spawn(&effect(Some("cargo test"), None), inputs())
            .unwrap();
        assert_eq!(handle.id, "machine-1");
        assert_eq!(
            executor.wait(&handle).unwrap(),
            EffectStatus::SettledRemotely
        );
        assert_eq!(
            *executor.launcher.waited.lock().unwrap(),
            vec!["machine-1".to_owned()]
        );
        let launched = executor.launcher.launched.lock().unwrap();
        assert_eq!(launched.len(), 1);
        assert_eq!(launched.first().unwrap().command, "cargo test");
    }
}
