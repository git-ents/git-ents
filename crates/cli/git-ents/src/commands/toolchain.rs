//! `git ents toolchain`: a thin wrapper around `ents_kiln::toolchain`'s
//! business logic — this module only resolves the signer/actor identity
//! against [`LocalRoot`] and translates a reached `Outcome` into a
//! CLI-facing [`Result`] (`crate::mutate::outcome_to_result`), exactly as
//! every other mutation command does.

use ents_kiln::{Recipe, Toolchain, toolchain};
use ents_receive::Identity;

use super::{actor, signer};
use crate::error::Result;
use crate::mutate::outcome_to_result;
use crate::root::LocalRoot;

/// `git ents toolchain list`: every toolchain name currently defined.
///
/// # Errors
///
/// Propagates a ref-store read failure.
pub fn list(root: &LocalRoot) -> Result<Vec<String>> {
    Ok(toolchain::list(&root.refs)?)
}

/// `git ents toolchain import`: embed `bin` whole as toolchain `name`.
///
/// # Errors
///
/// [`crate::error::Error::Effect`] if `bin` cannot be walked, or
/// serialization or `receive` itself fails; see
/// [`crate::mutate::outcome_to_result`] for how a reached refusal renders.
pub fn import(
    root: &LocalRoot,
    name: &str,
    bin: &std::path::Path,
    key: Option<std::path::PathBuf>,
) -> Result<()> {
    let signer = signer(root, key)?;
    let identity = Identity {
        actor: actor(&signer),
        author: None,
        sign: &|payload| signer.sign(payload),
    };
    let outcome = toolchain::import(
        &root.refs,
        &root.objects,
        &root.events,
        bin,
        name,
        &identity,
        root.mode(),
    )?;
    outcome_to_result(outcome, None)?;
    Ok(())
}

/// `git ents toolchain view`: the toolchain's recorded recipe.
///
/// # Errors
///
/// [`crate::error::Error::Effect`] (wrapping
/// [`ents_effect::Error::UnknownToolchain`]) if `name` has no toolchain
/// ref.
pub fn view(root: &LocalRoot, name: &str) -> Result<(Toolchain, Recipe)> {
    Ok(toolchain::view(&root.refs, &root.objects, name)?)
}

/// `git ents toolchain log`: every past import, newest first — the ref's
/// own commit log.
///
/// # Errors
///
/// [`crate::error::Error::Effect`] (wrapping [`ents_effect::Error::NotFound`])
/// if `name` has no toolchain ref.
pub fn log(root: &LocalRoot, name: &str) -> Result<Vec<gix_hash::ObjectId>> {
    Ok(toolchain::log(&root.refs, &root.objects, name)?)
}
