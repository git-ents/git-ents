//! `git ents toolchain`: import a local `bin/` directory as an embedded
//! toolchain manifest, view its provenance, and show its import history
//! (`model.toolchain`, `effect.toolchains`).
//!
//! Only [`ents_effect::Recipe::Embedded`] is wired here (`--from` recipes —
//! `rustup`, `sccache`, `url` — are `pre-redo` extras this phase's spec
//! does not name; deferred, see this crate's final report).

use std::path::Path;

use ents_effect::Recipe;
use ents_model::{Toolchain, namespace};
use gix_object::Tree;
use gix_object::tree::{Entry, EntryKind};
use gix_ref_store::RefStoreRead;

use super::{actor, signer};
use crate::error::{Error, Result};
use crate::mutate::{Identity, outcome_to_result, propose_entity};
use crate::root::LocalRoot;

/// `git ents toolchain import`: embed `bin` whole as toolchain `name`.
///
/// # Errors
///
/// [`Error::Io`] if `bin` cannot be walked; otherwise see
/// [`crate::mutate::outcome_to_result`].
pub fn import(
    root: &LocalRoot,
    name: &str,
    bin: &Path,
    key: Option<std::path::PathBuf>,
) -> Result<()> {
    let tree = write_dir_as_tree(bin, &root.objects)?;
    let recipe = Recipe::Embedded { tree };
    let toolchain = Toolchain {
        name: name.to_owned(),
        recipe: recipe.render(),
    };
    let signer = signer(root, key)?;
    let ref_name = namespace::toolchain_ref(name)?;
    let identity = Identity {
        actor: actor(&signer),
        signer: &signer,
    };
    let outcome = propose_entity(
        &root.refs,
        &root.objects,
        &root.events,
        ref_name,
        &toolchain,
        &identity,
        &format!("Import toolchain {name}"),
        root.mode(),
    )?;
    outcome_to_result(outcome, None)?;
    Ok(())
}

/// `git ents toolchain view`: the toolchain's recorded recipe.
///
/// # Errors
///
/// Propagates [`ents_effect::toolchain::resolve`]'s own errors.
pub fn view(root: &LocalRoot, name: &str) -> Result<(Toolchain, Recipe)> {
    Ok(ents_effect::toolchain::resolve(
        &root.refs,
        &root.objects,
        name,
    )?)
}

/// `git ents toolchain log`: every past import, newest first — the ref's
/// own commit log.
///
/// # Errors
///
/// [`Error::NotFound`] if `name` has no toolchain ref.
pub fn log(root: &LocalRoot, name: &str) -> Result<Vec<gix_hash::ObjectId>> {
    let ref_name = namespace::toolchain_ref(name)?;
    let repo = gix::open(&root.path)?;
    let Some(tip) = root.refs.get(ref_name.as_ref())? else {
        return Err(Error::NotFound {
            what: format!("toolchain {name}"),
        });
    };
    let mut out = Vec::new();
    let mut next = Some(tip);
    while let Some(oid) = next {
        out.push(oid);
        let commit = repo
            .find_object(oid)
            .map_err(|source| Error::InvalidArgument(source.to_string()))?
            .try_into_commit()
            .map_err(|_source| Error::InvalidArgument(format!("{oid} is not a commit")))?;
        next = commit.parent_ids().next().map(|id| id.detach());
    }
    Ok(out)
}

/// Recursively write `dir`'s contents into `objects` as a tree, preserving
/// the executable bit and recursing into subdirectories — the inverse of
/// `ents_effect::materialize::checkout`. Symlinks and anything that is not
/// a plain file or directory are refused (`anchor.retention`-style
/// defensiveness: a toolchain import should never silently embed something
/// that cannot round-trip through a tree).
///
/// # Errors
///
/// [`Error::Io`] on a read failure or an unsupported entry kind.
fn write_dir_as_tree(dir: &Path, objects: &impl gix_object::Write) -> Result<gix_hash::ObjectId> {
    let mut entries = Vec::new();
    let read = std::fs::read_dir(dir).map_err(|source| Error::Io {
        path: dir.to_owned(),
        source,
    })?;
    for item in read {
        let item = item.map_err(|source| Error::Io {
            path: dir.to_owned(),
            source,
        })?;
        let file_type = item.file_type().map_err(|source| Error::Io {
            path: item.path(),
            source,
        })?;
        let filename = item.file_name().to_string_lossy().into_owned();
        let (mode, oid) = if file_type.is_dir() {
            (EntryKind::Tree, write_dir_as_tree(&item.path(), objects)?)
        } else if file_type.is_file() {
            let bytes = std::fs::read(item.path()).map_err(|source| Error::Io {
                path: item.path(),
                source,
            })?;
            let executable = is_executable(&item.path());
            let kind = if executable {
                EntryKind::BlobExecutable
            } else {
                EntryKind::Blob
            };
            let oid = objects.write_buf(gix_object::Kind::Blob, &bytes)?;
            (kind, oid)
        } else {
            return Err(Error::Io {
                path: item.path(),
                source: std::io::Error::other("unsupported entry (symlink or special file)"),
            });
        };
        entries.push(Entry {
            mode: mode.into(),
            filename: filename.into(),
            oid,
        });
    }
    entries.sort();
    Ok(objects.write(&Tree { entries })?)
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .map(|meta| meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> bool {
    false
}
