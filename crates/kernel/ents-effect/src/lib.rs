//! Effect execution, results, and toolchains at run time (`docs/spec/effect.adoc`):
//! the `Executor` trait, its Docker and Sprite backends, toolchain
//! materialization, and the run loop that ties them to
//! [`ents_receive::receive`] as the sole path a result re-enters the
//! repository.
//!
//! This crate closes the loop `ents-gate` and `ents-receive` open
//! (`docs/abstractions.adoc`, "The loop"): an effect's trigger is
//! evaluated by `ents-query` (already linked by `ents-receive` for
//! footprint matching, never by this crate's own dependents in the other
//! direction — `arch.query-effect-split`), its run happens behind one
//! [`Executor`] seam with multiple backends, and its outcome returns as an
//! ordinary signed commit through [`write_result`], a `receive` client
//! exactly like the CLI or a web edit.
//!
//! # Spec coverage
//!
//! From `docs/spec/effect.adoc`:
//!
//! - `effect.definition`, `effect.admin-only` — already carried by
//!   `ents-model`'s [`ents_model::Effect`] and `ents-gate`'s default
//!   authorization arm; nothing new here.
//! - `effect.validation` — [`definition::validate`]. `ents-receive` cannot
//!   call this itself (`arch.query-effect-split`); a future frontend that
//!   builds an effect-definition commit does, before ever proposing the
//!   write.
//! - `effect.execution`, `effect.deployment-property` — [`Executor`],
//!   [`SandboxInputs`], [`RunOutput`]; [`docker::DockerExecutor`] (feature
//!   `docker`), [`sprite::SpriteExecutor`] (feature `sprite`),
//!   [`UnsandboxedExecutor`]. No executor, sandbox, or retry choice is
//!   readable from an [`ents_model::Effect`] — every backend is
//!   constructed and selected only by a composition root.
//! - `effect.local-run` — [`run::run_one`] is the single code path
//!   [`run::run_effect`] (the boot-time/on-demand form, no queue) and a
//!   future hosted worker (a queue drain feeding the same [`run::run_one`]
//!   calls) both use.
//! - `effect.results-writeback`, `effect.result-taxonomy` —
//!   [`write_result`]: an ordinary [`ents_receive::receive`] client,
//!   landing exactly `pass`/`fail`/`error` on
//!   `refs/meta/results/<effect>/<short-oid>`
//!   ([`run::short_oid`]). This crate never writes `Status::Error` itself
//!   — see [`Error`]'s own doc for why an infrastructure failure is always
//!   an `Err`, never a taxonomy value this crate chooses on a caller's
//!   behalf.
//! - `effect.identity` — [`write_result`] takes a `sign` closure the
//!   composition root injects (mirrors `ents_sync::resolve::merge_heads`);
//!   this crate never holds key material.
//! - `effect.official` — a refname-authorization rule on canonical
//!   `refs/meta/results/<effect>/*`, owned by `ents-gate`'s future
//!   Config-driven worker-key narrowing (see that crate's own doc); this
//!   crate only chooses *which* refname to target
//!   ([`run::run_one`]'s `results_ref`), never judges authorization.
//! - `effect.self-run` — [`write_result`] and [`run::run_effect`] accept
//!   any results refname, canonical or
//!   [`ents_model::namespace::self_result_ref`]; adopting a self-run
//!   result onto the canonical ref is `ents-sync`'s adoption merge
//!   (`gate.adoption-merge`, `sync.adoption-machinery`), unchanged by this
//!   crate.
//! - `effect.toolchains`, `model.toolchain` — moved to `ents-kiln`
//!   (`Toolchain`, `Recipe`, `Component`, `toolchain::resolve`,
//!   `toolchain::materialize`): this crate's own contract is now just that
//!   [`SandboxInputs::toolchains`] is a pre-materialized slice; resolving
//!   declared names to that slice is the composition root's job, done via
//!   `ents-kiln`. Only [`Executor::run`]'s sandbox ever touches the
//!   materialized bytes this crate hands it.
//! - `effect.fanout-index` — structurally satisfied, no dedicated code: a
//!   fanout-index rebuild is an ordinary effect (`run`ning `git index
//!   rebuild` or similar, a later, unbuilt command), so it uses exactly
//!   the same [`Executor`] and [`write_result`] path as any other effect.
//!
//! # Examples
//!
//! An end-to-end local run: enroll a worker, define an effect and a
//! (trivial, embedded-empty) toolchain, advance a code ref, and run the
//! effect with a stub executor — the shape `effect.local-run` names, minus
//! only a real sandbox.
//!
//! ```
//! use ents_effect::run::{run_effect, short_oid};
//! use ents_effect::{Executor, RunOutput, RunStatus, SandboxInputs};
//! use ents_model::{Effect, Provenance, namespace};
//! use ents_receive::{Mode, NullEventSink};
//! use ents_testutil::{Keypair, MemRefStore, ObjectStore, advance_ref, enroll_member};
//! use gix_ref_store::RefStoreRead as _;
//!
//! struct AlwaysPass;
//! impl Executor for AlwaysPass {
//!     fn run(&self, _inputs: &SandboxInputs<'_>) -> ents_effect::Result<RunOutput> {
//!         Ok(RunOutput { status: RunStatus::Pass, log: "ok".into() })
//!     }
//! }
//!
//! let refs = MemRefStore::default();
//! let objects = ObjectStore::default();
//! let worker = Keypair::from_seed(1);
//! enroll_member(&refs, &objects, "worker", &worker, Provenance::AdminRegistered, 100);
//!
//! // No declared toolchains: resolving names to materialized directories
//! // is the composition root's job (via `ents-kiln`), not this crate's —
//! // `run_effect` only ever receives an already-materialized slice.
//! let effect = Effect {
//!     name: "unit".into(),
//!     trigger: "rev(refs/heads/main)".into(),
//!     toolchains: vec![],
//!     run: "true".into(),
//! };
//! assert!(ents_effect::definition::validate(&effect).is_ok());
//!
//! let commits = advance_ref(&refs, &objects, "refs/heads/main", 1, 200);
//!
//! let author = gix::actor::Signature {
//!     name: "worker".into(), email: "worker@ents.test".into(),
//!     time: gix::date::Time { seconds: 300, offset: 0 },
//! };
//! let scratch = tempfile::tempdir().expect("tempdir");
//!
//! let outcomes = run_effect(
//!     &refs, &objects, &NullEventSink, &AlwaysPass, scratch.path(), &[],
//!     "unit", &effect, None,
//!     |short| Ok(namespace::result_ref("unit", short).expect("valid")),
//!     &author, &|payload| worker.sign(payload), Mode::Advisory,
//! ).expect("runs");
//!
//! assert_eq!(outcomes.len(), 1);
//! assert_eq!(outcomes[0].0, commits[0]);
//! assert_eq!(outcomes[0].1.result, ents_receive::TxResult::Applied);
//!
//! // The canonical results ref now carries a pass.
//! let name = namespace::result_ref("unit", &short_oid(commits[0])).expect("valid");
//! assert!(refs.get(name.as_ref()).expect("readable").is_some());
//! ```

pub mod definition;
mod error;
pub mod executor;
pub mod materialize;
mod results;
pub mod run;
mod unsandboxed;

#[cfg(feature = "docker")]
pub mod docker;
#[cfg(feature = "sprite")]
pub mod sprite;

pub use error::{Error, Result};
pub use executor::{Executor, RunOutput, RunStatus, SandboxInputs};
pub use results::write_result;
pub use run::{run_effect, run_one};
pub use unsandboxed::UnsandboxedExecutor;

#[cfg(feature = "docker")]
pub use docker::DockerExecutor;
#[cfg(feature = "sprite")]
pub use sprite::SpriteExecutor;
