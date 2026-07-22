//! `git ents config`: forge-wide, non-secret agent-runtime defaults
//! (provider name, default model) recorded in `refs/meta/config` alongside
//! the gate's own `epoch`/`workers` fields (`ents_gate::Config`).
//!
//! The API token is deliberately not here and never will be: it lives only
//! in the deployment-time credential seam (`crate::credentials`,
//! `GIT_ENTS_CREDENTIALS_FILE`), never in a signed, replicated,
//! multi-reader tree.

use ents_gate::Config;
use ents_model::namespace;
use ents_receive::{Identity, propose_entity};
use gix_ref_store::RefStoreRead;

use super::{actor, signer};
use crate::error::Result;
use crate::mutate::outcome_to_result;
use crate::root::LocalRoot;

/// `git ents config show`: this repository's current forge-wide
/// configuration -- [`Config::default`] (every field unset) when
/// `refs/meta/config` has no tip yet, the same "absent means unconfigured"
/// reading every `ents_gate::config` reader already gives.
///
/// # Errors
///
/// Propagates a ref-store or object read failure, or an unreadable config
/// tree.
pub fn show(root: &LocalRoot) -> Result<Config> {
    #[expect(
        clippy::expect_used,
        clippy::unwrap_in_result,
        reason = "CONFIG_REF is a fixed, compile-time-known-valid refname literal"
    )]
    let name: gix::refs::FullName = namespace::CONFIG_REF
        .try_into()
        .expect("fixed, valid refname");
    let Some(tip) = root.refs.get(name.as_ref())? else {
        return Ok(Config::default());
    };
    let tree = super::commit_tree(&root.objects, tip)?;
    Ok(facet_git_tree::deserialize::<Config>(&tree, &root.objects)?)
}

/// `git ents config set`: narrow the agent-runtime defaults. Each argument
/// is an independent optional narrowing -- omit one to leave whatever it
/// currently holds untouched, rather than resetting it to `None`. Reads
/// the current config first and writes the merged whole back, so a `set`
/// naming only `agent_provider` cannot clobber an `agent_default_model`
/// set earlier, or the gate's own `epoch`/`workers`.
///
/// # Errors
///
/// Propagates a ref-store/object read failure or a signing failure;
/// otherwise see [`crate::mutate::outcome_to_result`] for how a reached
/// refusal renders.
pub fn set(
    root: &LocalRoot,
    agent_provider: Option<String>,
    agent_default_model: Option<String>,
    key: Option<std::path::PathBuf>,
) -> Result<()> {
    let signer = signer(root, key)?;
    let mut config = show(root)?;
    if let Some(provider) = agent_provider {
        config.agent_provider = Some(provider);
    }
    if let Some(model) = agent_default_model {
        config.agent_default_model = Some(model);
    }
    #[expect(
        clippy::expect_used,
        clippy::unwrap_in_result,
        reason = "CONFIG_REF is a fixed, compile-time-known-valid refname literal"
    )]
    let name: gix::refs::FullName = namespace::CONFIG_REF
        .try_into()
        .expect("fixed, valid refname");
    let identity = Identity {
        actor: actor(&signer),
        author: None,
        sign: &|payload| signer.sign(payload),
    };
    let outcome = propose_entity(
        &root.refs,
        &root.objects,
        &root.events,
        name,
        &config,
        &identity,
        "Set agent config",
        root.mode(),
    )?;
    outcome_to_result(outcome, None)?;
    Ok(())
}
