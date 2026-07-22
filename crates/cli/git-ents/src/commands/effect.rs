//! `git ents effect`: define, list, show, run, and log effects
//! (`model.effect-definition`, `effect.local-run`).

use ents_effect::run::{run_effect, short_oid};
use ents_model::{Effect, ResultRecord, Status, namespace};
use ents_receive::{Identity, propose_entity};
use gix_ref_store::RefStoreRead;

use super::{actor, signer};
use crate::error::{Error, Result};
use crate::mutate::outcome_to_result;
use crate::root::LocalRoot;

/// `git ents effect list`: every effect currently defined.
///
/// # Errors
///
/// Propagates a ref-store or object read failure.
pub fn list(root: &LocalRoot) -> Result<Vec<(String, Effect)>> {
    let mut out = Vec::new();
    for entry in root.refs.iter_prefix("refs/meta/effects/")? {
        let (name, tip) = entry?;
        let path = name.as_bstr().to_string();
        let Some(short) = path.strip_prefix("refs/meta/effects/") else {
            continue;
        };
        if short.is_empty() || short.contains('/') {
            continue;
        }
        let tree = super::commit_tree(&root.objects, tip)?;
        if let Ok(effect) = facet_git_tree::deserialize::<Effect>(&tree, &root.objects) {
            out.push((short.to_owned(), effect));
        }
    }
    Ok(out)
}

/// `git ents effect add`: define (or replace) `name`.
///
/// # Errors
///
/// See [`crate::mutate::outcome_to_result`].
pub fn add(
    root: &LocalRoot,
    name: &str,
    on: String,
    run: String,
    toolchains: Vec<String>,
    key: Option<std::path::PathBuf>,
) -> Result<()> {
    // Validate the trigger parses before it is ever written — a malformed
    // trigger would otherwise be silently skipped by every future
    // reconciliation scan (`ents_receive::reconcile`'s own tolerance rule).
    let _: ents_query::Query = on
        .parse()
        .map_err(|_source| Error::InvalidArgument(format!("unparsable trigger: {on}")))?;

    let signer = signer(root, key)?;
    let effect = Effect {
        name: name.to_owned(),
        trigger: on,
        toolchains,
        run,
    };
    let ref_name = namespace::effect_ref(name)?;
    let identity = Identity {
        actor: actor(&signer),
        author: None,
        sign: &|payload| signer.sign(payload),
    };
    let outcome = propose_entity(
        &root.refs,
        &root.objects,
        &root.events,
        ref_name,
        &effect,
        &identity,
        &format!("Define effect {name}"),
        root.mode(),
    )?;
    outcome_to_result(outcome, None)?;
    Ok(())
}

/// `git ents effect show`: the definition, plus its result at `at` when
/// given.
///
/// # Errors
///
/// [`Error::NotFound`] if `name` has no effect definition.
pub fn show(root: &LocalRoot, name: &str, at: Option<String>) -> Result<(Effect, Option<Status>)> {
    let ref_name = namespace::effect_ref(name)?;
    let Some(tip) = root.refs.get(ref_name.as_ref())? else {
        return Err(Error::NotFound {
            what: format!("effect {name}"),
        });
    };
    let tree = super::commit_tree(&root.objects, tip)?;
    let effect = facet_git_tree::deserialize::<Effect>(&tree, &root.objects)?;

    let status = match at {
        None => None,
        Some(commit) => {
            let oid = resolve_commit(root, &commit)?;
            let results_ref = namespace::result_ref(name, &short_oid(oid))?;
            match root.refs.get(results_ref.as_ref())? {
                None => None,
                Some(result_tip) => {
                    let tree = super::commit_tree(&root.objects, result_tip)?;
                    facet_git_tree::deserialize::<ResultRecord>(&tree, &root.objects)
                        .ok()
                        .map(|record| record.status)
                }
            }
        }
    };
    Ok((effect, status))
}

/// `git ents effect run`: run `name` locally against every outstanding
/// commit, or a single `at` — no queue, identical materialization and
/// sandbox path to a hosted worker (`effect.local-run`).
///
/// # Errors
///
/// Propagates any failure `ents_effect::run::run_effect` reports.
#[expect(
    clippy::result_large_err,
    reason = "the closure passed to run_effect below is typed against ents_effect::Error, that \
              crate's own Result shape, not this crate's to box"
)]
pub fn run(
    root: &LocalRoot,
    name: &str,
    at: Option<String>,
    key: Option<std::path::PathBuf>,
    executor: &dyn ents_effect::Executor,
) -> Result<Vec<(gix_hash::ObjectId, ents_receive::Outcome)>> {
    let ref_name = namespace::effect_ref(name)?;
    let Some(tip) = root.refs.get(ref_name.as_ref())? else {
        return Err(Error::NotFound {
            what: format!("effect {name}"),
        });
    };
    let tree = super::commit_tree(&root.objects, tip)?;
    let effect = facet_git_tree::deserialize::<Effect>(&tree, &root.objects)?;

    let signer = signer(root, key)?;
    let at_oid = at.map(|rev| resolve_commit(root, &rev)).transpose()?;

    let scratch = tempfile::tempdir().map_err(|source| Error::Io {
        path: root.path.clone(),
        source,
    })?;
    let cache = tempfile::tempdir().map_err(|source| Error::Io {
        path: root.path.clone(),
        source,
    })?;

    // `run_effect` no longer resolves toolchain names itself: resolve and
    // materialize each of the effect's declared toolchains here, before
    // handing the run loop an already-materialized slice.
    let mut toolchains = Vec::with_capacity(effect.toolchains.len());
    for toolchain_name in &effect.toolchains {
        let (_, recipe) = ents_kiln::toolchain::resolve(&root.refs, &root.objects, toolchain_name)?;
        let bin = ents_kiln::toolchain::materialize(&recipe, &root.objects, cache.path())?;
        toolchains.push((toolchain_name.clone(), bin));
    }

    let author = actor(&signer);
    let outcomes = run_effect(
        &root.refs,
        &root.objects,
        &root.events,
        executor,
        scratch.path(),
        &toolchains,
        name,
        &effect,
        at_oid,
        |short| canonical_result_ref(name, short),
        &author,
        &|payload| signer.sign(payload),
        root.mode(),
    )?;
    Ok(outcomes)
}

/// Build the canonical results refname for one run, in the shape
/// `run_effect`'s own `results_ref` parameter expects.
///
/// # Errors
///
/// Never in practice: `name` is an already-defined effect and `short` is
/// always a hex oid slice ([`ents_effect::run::short_oid`]'s own shape), so
/// both always compose into a well-formed refname; kept fallible only
/// because `ents_effect::run::run_effect`'s own signature requires it.
#[expect(
    clippy::result_large_err,
    reason = "the Result shape is ents_effect::run_effect's own signature, not this crate's to box"
)]
fn canonical_result_ref(name: &str, short: &str) -> ents_effect::Result<gix::refs::FullName> {
    #[expect(
        clippy::expect_used,
        clippy::unwrap_in_result,
        reason = "see this function's own doc: always well-formed in practice"
    )]
    Ok(namespace::result_ref(name, short).expect("well-formed refname segments"))
}

/// `git ents effect log`: every recorded result for `name`, keyed by the
/// full oid of the judged commit (the identity `model.result-identity`
/// binds, and what `results(...)` queries match on).
///
/// # Errors
///
/// Propagates a ref-store or object read failure.
pub fn log(root: &LocalRoot, name: &str) -> Result<Vec<(gix_hash::ObjectId, ResultRecord)>> {
    let prefix = format!("refs/meta/results/{name}/");
    let mut out = Vec::new();
    for entry in root.refs.iter_prefix(&prefix)? {
        let (_, tip) = entry?;
        let tree = super::commit_tree(&root.objects, tip)?;
        if let Ok(record) = facet_git_tree::deserialize::<ResultRecord>(&tree, &root.objects) {
            out.push((record.target(), record));
        }
    }
    Ok(out)
}

fn resolve_commit(root: &LocalRoot, rev: &str) -> Result<gix_hash::ObjectId> {
    let repo = gix::open(&root.path)?;
    let id = repo
        .rev_parse_single(rev)
        .map_err(|source| Error::InvalidArgument(format!("cannot resolve {rev}: {source}")))?;
    Ok(id.detach())
}
