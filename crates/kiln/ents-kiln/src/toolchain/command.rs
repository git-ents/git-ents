//! `git ents toolchain`: import a local `bin/` directory as an embedded
//! toolchain manifest, view its provenance, and show its import history
//! (`model.toolchain`, `effect.toolchains`).
//!
//! Generalized over the same trait-object/generic seam `ents_effect::run`
//! uses (`&dyn RefStore`/`RefStoreRead`, `impl Find`/`Find + Write`, `&dyn
//! ents_receive::EventSink`) rather than any concrete composition-root
//! type, so this crate never depends on a CLI or a specific store
//! implementation — a composition root wires the concrete types and calls
//! these functions, never the other way around.
//!
//! Only [`super::Recipe::Embedded`] is wired here (`--from` recipes —
//! `rustup`, `sccache`, `url` — are `pre-redo` extras this phase's spec
//! does not name; deferred, see this crate's own final report).
#![expect(
    clippy::result_large_err,
    reason = "every function here returns ents_effect::Result — that crate's own Error type, \
              reused as-is rather than wrapped, since the toolchain domain's errors already live \
              naturally in its enum; not this crate's to box"
)]

use std::path::Path;

use ents_effect::{Error, Result};
use ents_model::namespace;
use ents_receive::{EventSink, Identity, Mode, Outcome, propose_entity};
use gix_hash::ObjectId;
use gix_object::tree::{Entry, EntryKind};
use gix_object::{CommitRef, Find, Kind, Tree, Write};
use gix_ref_store::{RefStore, RefStoreRead};

use super::{Recipe, Toolchain, resolve};

/// `git ents toolchain list`: every toolchain name currently defined.
///
/// # Errors
///
/// Propagates a ref-store read failure.
///
/// # Examples
///
/// ```
/// use ents_kiln::toolchain::list;
/// use ents_testutil::MemRefStore;
///
/// let refs = MemRefStore::default();
/// assert!(list(&refs).expect("reads").is_empty());
/// ```
pub fn list(refs: &dyn RefStoreRead) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for entry in refs.iter_prefix("refs/meta/toolchains/")? {
        let (name, _) = entry?;
        let path = name.as_bstr().to_string();
        if let Some(rest) = path.strip_prefix("refs/meta/toolchains/") {
            out.push(rest.to_owned());
        }
    }
    Ok(out)
}

/// `git ents toolchain import`: embed `bin` whole as toolchain `name`.
///
/// Returns the raw [`Outcome`] `receive` reached — callers interpret it
/// themselves (the CLI's own `outcome_to_result`, for instance), the same
/// shape `ents_effect::run::run_one` and `ents_forge::comment::add` return
/// their own raw `Outcome` in.
///
/// # Errors
///
/// [`Error::Io`] if `bin` cannot be walked; otherwise propagates
/// serialization or `receive` failures.
pub fn import(
    refs: &dyn RefStore,
    objects: &(impl Find + Write),
    events: &dyn EventSink,
    bin: &Path,
    name: &str,
    identity: &Identity<'_>,
    mode: Mode,
) -> Result<Outcome> {
    let tree = write_dir_as_tree(bin, objects)?;
    let recipe = Recipe::Embedded { tree };
    let toolchain = Toolchain {
        name: name.to_owned(),
        recipe: recipe.render(),
    };
    let ref_name = namespace::toolchain_ref(name)
        .map_err(|_invalid| Error::InvalidToolchainName(name.to_owned()))?;
    let outcome = propose_entity(
        refs,
        objects,
        events,
        ref_name,
        &toolchain,
        identity,
        &format!("Import toolchain {name}"),
        mode,
    )?;
    Ok(outcome)
}

/// `git ents toolchain view`: the toolchain's recorded recipe.
///
/// # Errors
///
/// Propagates [`resolve`]'s own errors.
pub fn view(
    refs: &dyn RefStoreRead,
    objects: &impl Find,
    name: &str,
) -> Result<(Toolchain, Recipe)> {
    resolve(refs, objects, name)
}

/// `git ents toolchain log`: every past import, newest first — the ref's
/// own commit log (first-parent chain).
///
/// # Errors
///
/// [`Error::NotFound`] if `name` has no toolchain ref; [`Error::Decode`] if
/// a commit in the chain cannot be read.
pub fn log(refs: &dyn RefStoreRead, objects: &impl Find, name: &str) -> Result<Vec<ObjectId>> {
    let ref_name = namespace::toolchain_ref(name)
        .map_err(|_invalid| Error::InvalidToolchainName(name.to_owned()))?;
    let Some(tip) = refs.get(ref_name.as_ref())? else {
        return Err(Error::NotFound {
            what: format!("toolchain {name}"),
        });
    };
    let mut out = Vec::new();
    let mut next = Some(tip);
    while let Some(oid) = next {
        out.push(oid);
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
        next = commit.parents().next();
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
fn write_dir_as_tree(dir: &Path, objects: &impl Write) -> Result<ObjectId> {
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
        let filename = item.file_name().into_string().map_err(|raw| Error::Io {
            path: dir.join(raw),
            source: std::io::Error::other("non-UTF-8 filename cannot round-trip through a tree"),
        })?;
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
