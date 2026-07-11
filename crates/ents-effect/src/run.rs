//! The run loop (`effect.execution`, `effect.local-run`): materialize an
//! effect's declared toolchains and the tested commit's tree, hand both to
//! an [`Executor`], and write the outcome back through
//! [`crate::write_result`].
//!
//! [`run_one`] is the one code path a hosted worker and `git effect run`
//! both call (`effect.local-run`: "the identical code path a hosted worker
//! uses"); only what surrounds it differs — a durable queue feeding a
//! worker's loop of [`run_one`] calls, versus [`run_effect`] deriving the
//! same obligations directly from [`ents_query::Evaluator::outstanding`]
//! and calling [`run_one`] once per commit, with no queue at all
//! (`effect.local-run`: "only the durable queue MUST be skipped").

use std::path::Path;

use ents_model::Effect;
use ents_query::{Evaluator, Query};
use ents_receive::{EventSink, Mode, Outcome};
use gix::refs::FullName;
use gix_hash::ObjectId;
use gix_object::{CommitRef, Find, Kind, Write};
use gix_ref_store::RefStore;

use crate::error::{Error, Result};
use crate::executor::{Executor, RunStatus, SandboxInputs};
use crate::results::write_result;
use crate::toolchain;

/// The tree of the commit at `oid`.
fn commit_tree(objects: &impl Find, oid: ObjectId) -> Result<ObjectId> {
    let mut buf = Vec::new();
    let data = objects
        .try_find(&oid, &mut buf)
        .map_err(|source| Error::Decode {
            oid,
            detail: source.to_string(),
        })?
        .ok_or(Error::Missing { oid })?;
    if data.kind != Kind::Commit {
        return Err(Error::Decode {
            oid,
            detail: "expected a commit".to_owned(),
        });
    }
    let commit = CommitRef::from_bytes(data.data, oid.kind()).map_err(|e| Error::Decode {
        oid,
        detail: e.to_string(),
    })?;
    Ok(commit.tree())
}

/// The short-oid segment convention every results refname uses:
/// `refs/meta/results/<effect>/<short-oid>` (`effect.results-writeback`) —
/// the first 12 hex characters, long enough to stay unambiguous within one
/// effect's results namespace while keeping refnames short.
///
/// # Examples
///
/// ```
/// use ents_effect::run::short_oid;
///
/// let oid = gix_hash::ObjectId::null(gix_hash::Kind::Sha1);
/// assert_eq!(short_oid(oid), "000000000000");
/// ```
#[must_use]
pub fn short_oid(oid: ObjectId) -> String {
    let hex = oid.to_string();
    hex.get(..12).unwrap_or(&hex).to_owned()
}

/// Materialize `effect`'s declared toolchains against `toolchain_cache`,
/// returning each name paired with its host `bin/` directory, in the
/// effect's declared order (`crate::executor::activate`'s PATH-collision
/// tiebreak depends on this order surviving).
///
/// # Errors
///
/// [`Error::UnknownToolchain`] or [`Error::InvalidRecipe`] for a name the
/// effect declares but that does not resolve; see
/// [`crate::toolchain::materialize`] for extraction failures.
fn resolve_toolchains(
    refs: &dyn gix_ref_store::RefStoreRead,
    objects: &impl Find,
    effect: &Effect,
    toolchain_cache: &Path,
) -> Result<Vec<(String, std::path::PathBuf)>> {
    let mut out = Vec::with_capacity(effect.toolchains.len());
    for name in &effect.toolchains {
        let (_, recipe) = toolchain::resolve(refs, objects, name)?;
        let bin = toolchain::materialize(&recipe, objects, toolchain_cache)?;
        out.push((name.clone(), bin));
    }
    Ok(out)
}

/// Run `effect` against the single commit `oid`: materialize its
/// toolchains and `oid`'s tree, execute via `executor`, and write the
/// outcome to `results_ref` — the one code path `effect.local-run` names.
///
/// `results_ref` is the caller's choice (`effect.self-run`,
/// `effect.official`): the canonical results ref for a designated worker,
/// or a self-run mirror for any other member. `scratch` holds the
/// per-run, never-cached tree checkout (a Docker container is thrown away
/// per run; a Sprite's `sync_dir` re-syncs it every time
/// too, so nothing here needs it to survive); `toolchain_cache` holds the
/// extract-once toolchain cache [`crate::toolchain::materialize`] shares
/// across runs.
///
/// # Errors
///
/// Any [`Error`] from resolving or materializing a toolchain, checking out
/// `oid`'s tree, the executor itself, or [`crate::write_result`].
///
/// # Examples
///
/// ```
/// use ents_effect::run::run_one;
/// use ents_effect::{Executor, RunOutput, RunStatus, SandboxInputs};
/// use ents_model::{Effect, Provenance, namespace};
/// use ents_receive::{Mode, NullEventSink};
/// use ents_testutil::{Keypair, MemRefStore, ObjectStore, advance_ref, enroll_member};
///
/// struct AlwaysPass;
/// impl Executor for AlwaysPass {
///     fn run(&self, _inputs: &SandboxInputs<'_>) -> ents_effect::Result<RunOutput> {
///         Ok(RunOutput { status: RunStatus::Pass, log: String::new() })
///     }
/// }
///
/// let refs = MemRefStore::default();
/// let objects = ObjectStore::default();
/// let worker = Keypair::from_seed(1);
/// enroll_member(&refs, &objects, "worker", &worker, Provenance::AdminRegistered, 100);
/// let commits = advance_ref(&refs, &objects, "refs/heads/main", 1, 200);
///
/// let effect = Effect { trigger: "rev(refs/heads/main)".into(), toolchains: vec![], run: "true".into() };
/// let results_ref = namespace::result_ref("unit", "abcabcabcabc").expect("valid");
/// let author = gix::actor::Signature {
///     name: "worker".into(), email: "worker@ents.test".into(),
///     time: gix::date::Time { seconds: 300, offset: 0 },
/// };
/// let scratch = tempfile::tempdir().expect("tempdir");
/// let cache = tempfile::tempdir().expect("tempdir");
///
/// let outcome = run_one(
///     &refs, &objects, &NullEventSink, &AlwaysPass, scratch.path(), cache.path(),
///     commits[0], &effect, results_ref, &author, |p| worker.sign(p), Mode::Advisory,
/// ).expect("runs");
/// assert_eq!(outcome.result, ents_receive::TxResult::Applied);
/// ```
// @relation(effect.execution, effect.local-run, effect.toolchains, scope=function)
#[expect(
    clippy::too_many_arguments,
    reason = "one input per materialization step, mirrors pre-redo's engine::run shape"
)]
pub fn run_one(
    refs: &dyn RefStore,
    objects: &(impl Find + Write),
    events: &dyn EventSink,
    executor: &dyn Executor,
    scratch: &Path,
    toolchain_cache: &Path,
    oid: ObjectId,
    effect: &Effect,
    results_ref: FullName,
    author: &gix::actor::Signature,
    sign: impl FnOnce(&[u8]) -> String,
    mode: Mode,
) -> Result<Outcome> {
    let toolchains = resolve_toolchains(refs, objects, effect, toolchain_cache)?;

    let workdir = scratch.join(oid.to_string());
    std::fs::create_dir_all(&workdir).map_err(|source| Error::Io {
        path: workdir.clone(),
        source,
    })?;
    let tree = commit_tree(objects, oid)?;
    crate::materialize::checkout(objects, tree, &workdir)?;

    let inputs = SandboxInputs {
        workdir: &workdir,
        toolchains: &toolchains,
        command: &effect.run,
    };
    let output = executor.run(&inputs)?;
    let status = match output.status {
        RunStatus::Pass => ents_model::Status::Pass,
        RunStatus::Fail => ents_model::Status::Fail,
    };

    write_result(
        refs,
        objects,
        events,
        results_ref,
        status,
        author,
        sign,
        mode,
    )
}

/// Run `effect` against every commit currently owed a result
/// (`ents_query::Evaluator::outstanding`, `query.workset`), or against the
/// single commit `at` when given — the boot-time/on-demand form
/// [`run_one`]'s doc names, and the shape `git effect run [--at <commit>]`
/// (a future frontend) calls.
///
/// `results_ref` builds each run's target refname from its short oid
/// (`crate::run::short_oid`) — pass `ents_model::namespace::result_ref` for
/// a canonical worker or `ents_model::namespace::self_result_ref` curried
/// to one member for a self-run (`effect.self-run`); this function makes
/// no canonical-vs-self decision itself.
///
/// # Errors
///
/// [`Error::Eval`] if the work set cannot be computed; otherwise anything
/// [`run_one`] can fail with, for the first commit that fails — later
/// commits in the set are not attempted once one fails, since a caller
/// wrapping this in its own retry policy (`effect.deployment-property`)
/// needs to know exactly which commit stopped the batch.
// @relation(effect.local-run, query.workset, scope=function)
#[expect(
    clippy::too_many_arguments,
    reason = "one input per materialization step plus the target-ref builder"
)]
pub fn run_effect(
    refs: &dyn RefStore,
    objects: &(impl Find + Write),
    events: &dyn EventSink,
    executor: &dyn Executor,
    scratch: &Path,
    toolchain_cache: &Path,
    effect_name: &str,
    effect: &Effect,
    at: Option<ObjectId>,
    results_ref: impl Fn(&str) -> Result<FullName>,
    author: &gix::actor::Signature,
    sign: &impl Fn(&[u8]) -> String,
    mode: Mode,
) -> Result<Vec<(ObjectId, Outcome)>> {
    let trigger: Query = effect.trigger.parse()?;
    let oids: Vec<ObjectId> = match at {
        Some(oid) => vec![oid],
        None => {
            // `query.workset`'s dedup marker is always the effect's own
            // *canonical* results namespace, regardless of which ref this
            // particular run's outcome ends up targeting
            // (`results_ref`) — a self-run mirror never discharges the
            // canonical obligation, by construction.
            let evaluator = Evaluator::new(refs, objects);
            evaluator
                .outstanding(effect_name, &trigger)?
                .into_iter()
                .collect()
        }
    };

    let mut outcomes = Vec::with_capacity(oids.len());
    for oid in oids {
        let target = results_ref(&short_oid(oid))?;
        let outcome = run_one(
            refs,
            objects,
            events,
            executor,
            scratch,
            toolchain_cache,
            oid,
            effect,
            target,
            author,
            sign,
            mode,
        )?;
        outcomes.push((oid, outcome));
    }
    Ok(outcomes)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "unit test")]

    use ents_model::{Provenance, namespace};
    use ents_receive::{NullEventSink, TxResult};
    use ents_testutil::{Keypair, MemRefStore, ObjectStore, advance_ref, enroll_member};
    use gix_ref_store::RefStoreRead as _;
    use rstest::rstest;

    use super::*;
    use crate::executor::{RunOutput, RunStatus, SandboxInputs};

    struct AlwaysPass;
    impl Executor for AlwaysPass {
        fn run(&self, _inputs: &SandboxInputs<'_>) -> Result<RunOutput> {
            Ok(RunOutput {
                status: RunStatus::Pass,
                log: String::new(),
            })
        }
    }

    fn author() -> gix::actor::Signature {
        gix::actor::Signature {
            name: "worker".into(),
            email: "worker@ents.test".into(),
            time: gix::date::Time {
                seconds: 500,
                offset: 0,
            },
        }
    }

    #[rstest]
    // @relation(effect.local-run, query.workset, scope=function, role=Verifies)
    fn run_effect_derives_the_full_outstanding_set_with_no_queue_at_all() {
        let refs = MemRefStore::default();
        let objects = ObjectStore::default();
        let worker = Keypair::from_seed(1);
        enroll_member(
            &refs,
            &objects,
            "worker",
            &worker,
            Provenance::AdminRegistered,
            100,
        );
        let commits = advance_ref(&refs, &objects, "refs/heads/main", 2, 200);

        let effect = Effect {
            trigger: "rev(refs/heads/main)".into(),
            toolchains: vec![],
            run: "true".into(),
        };
        let scratch = tempfile::tempdir().expect("tempdir");
        let cache = tempfile::tempdir().expect("tempdir");

        // `NullEventSink`: the only component `effect.local-run` says this
        // path skips is the durable queue, and this run derives its work
        // set directly from `query.workset` instead of draining one.
        let outcomes = run_effect(
            &refs,
            &objects,
            &NullEventSink,
            &AlwaysPass,
            scratch.path(),
            cache.path(),
            "unit",
            &effect,
            None,
            |short| Ok(namespace::result_ref("unit", short).expect("valid")),
            &author(),
            &|payload| worker.sign(payload),
            Mode::Advisory,
        )
        .expect("runs");

        assert_eq!(outcomes.len(), 2);
        let mut ran: Vec<_> = outcomes.iter().map(|(oid, _)| *oid).collect();
        ran.sort();
        let mut expected = commits.clone();
        expected.sort();
        assert_eq!(ran, expected);
        for (_, outcome) in &outcomes {
            assert_eq!(outcome.result, TxResult::Applied);
        }
    }

    #[rstest]
    // @relation(effect.local-run, scope=function, role=Verifies)
    fn run_effect_at_a_single_commit_skips_the_work_set_scan() {
        let refs = MemRefStore::default();
        let objects = ObjectStore::default();
        let worker = Keypair::from_seed(1);
        enroll_member(
            &refs,
            &objects,
            "worker",
            &worker,
            Provenance::AdminRegistered,
            100,
        );
        let commits = advance_ref(&refs, &objects, "refs/heads/main", 3, 200);

        let effect = Effect {
            trigger: "rev(refs/heads/main)".into(),
            toolchains: vec![],
            run: "true".into(),
        };
        let scratch = tempfile::tempdir().expect("tempdir");
        let cache = tempfile::tempdir().expect("tempdir");

        let first = *commits.first().expect("advance_ref produced a commit");
        let outcomes = run_effect(
            &refs,
            &objects,
            &NullEventSink,
            &AlwaysPass,
            scratch.path(),
            cache.path(),
            "unit",
            &effect,
            Some(first),
            |short| Ok(namespace::result_ref("unit", short).expect("valid")),
            &author(),
            &|payload| worker.sign(payload),
            Mode::Advisory,
        )
        .expect("runs");

        assert_eq!(outcomes.len(), 1);
        let (oid, _) = outcomes.first().expect("one outcome");
        assert_eq!(*oid, first);
    }

    #[rstest]
    // @relation(effect.self-run, effect.local-run, scope=function, role=Verifies)
    fn run_effect_can_target_the_self_run_namespace_via_its_results_ref_closure() {
        let refs = MemRefStore::default();
        let objects = ObjectStore::default();
        let bob = Keypair::from_seed(2);
        enroll_member(
            &refs,
            &objects,
            "bob",
            &bob,
            Provenance::AdminRegistered,
            100,
        );
        let commits = advance_ref(&refs, &objects, "refs/heads/main", 1, 200);

        let effect = Effect {
            trigger: "rev(refs/heads/main)".into(),
            toolchains: vec![],
            run: "true".into(),
        };
        let scratch = tempfile::tempdir().expect("tempdir");
        let cache = tempfile::tempdir().expect("tempdir");
        let member = ents_model::MemberId::new("bob");

        let outcomes = run_effect(
            &refs,
            &objects,
            &NullEventSink,
            &AlwaysPass,
            scratch.path(),
            cache.path(),
            "unit",
            &effect,
            None,
            |short| Ok(namespace::self_result_ref(&member, "unit", short).expect("valid")),
            &author(),
            &|payload| bob.sign(payload),
            Mode::Advisory,
        )
        .expect("runs");

        assert_eq!(outcomes.len(), 1);
        let first = *commits.first().expect("advance_ref produced a commit");
        let name = namespace::self_result_ref(&member, "unit", &short_oid(first)).expect("valid");
        assert!(refs.get(name.as_ref()).expect("readable").is_some());
    }
}
