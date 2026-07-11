//! Toolchain resolution and materialization (`effect.toolchains`,
//! `model.toolchain`).
//!
//! [`ents_model::Toolchain::recipe`] is deliberately an opaque `String` —
//! `model.toolchain`'s own doc names this crate as the one that gives it
//! structure. [`Recipe`] is that structure: a toolchain's `bin` is either
//! [`Recipe::Embedded`] (a tree already in the object database, captured
//! whole by whatever wrote the toolchain) or [`Recipe::Downloaded`] (a set
//! of externally-hosted, sha256-pinned archives), ported from `pre-redo`'s
//! `git_toolchain::Bin` — the design pre-redo settled on and this phase
//! carries forward, not a fresh design. [`Recipe::render`]/[`Recipe::parse`]
//! round-trip it through the plain-text `recipe` field.
//!
//! [`materialize`] resolves a toolchain to a host directory containing its
//! activated `bin/`, extract-once cached under a content key (a tree oid
//! for `Embedded`, a hash of each component's pin for `Downloaded`) so a
//! backend that runs the same toolchain repeatedly (a Sprite's persistent
//! filesystem, a developer's local cache) never re-extracts unchanged
//! bytes. Fetching a [`Recipe::Downloaded`] component shells to `curl`,
//! `tar`, and `sha256sum`/`shasum`, the same pattern `pre-redo` used and
//! this phase ports rather than adding an HTTP or hashing dependency.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use ents_model::{Toolchain, namespace};
use gix_hash::ObjectId;
use gix_object::{CommitRef, Find, Kind};
use gix_ref_store::RefStoreRead;

use crate::error::{Error, Result};

/// How a toolchain's `bin` is provisioned — the structure inside
/// [`ents_model::Toolchain::recipe`] (`effect.toolchains`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recipe {
    /// `bin`'s directory tree, already in the object database — the whole
    /// tree's entries become the toolchain's activated `bin/` contents.
    Embedded {
        /// The tree object id.
        tree: ObjectId,
    },
    /// A set of archives fetched, sha256-verified, and merged onto disk at
    /// materialization time.
    Downloaded {
        /// Each archive making up the toolchain.
        components: Vec<Component>,
    },
}

/// One archive making up a [`Recipe::Downloaded`] toolchain: fetched from
/// `url` and checked against `sha256` before being extracted per
/// `strip`/`dest` — ported verbatim from `pre-redo`'s
/// `git_toolchain::Component`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Component {
    /// Where to fetch the archive from.
    pub url: String,
    /// The archive's expected sha256, hex-encoded.
    pub sha256: String,
    /// Leading path segments `tar` strips at extraction.
    pub strip: u8,
    /// Subdirectory under the toolchain's `bin/` extraction root to extract
    /// into: empty for an archive that already carries its own `bin/` top
    /// level, `bin` for a flat archive whose payload should itself land on
    /// `PATH`.
    pub dest: String,
}

const EMBEDDED: &str = "embedded";
const DOWNLOADED: &str = "downloaded";

impl Recipe {
    /// Parse a [`Recipe`] out of a [`ents_model::Toolchain::recipe`] string.
    ///
    /// The format is deliberately small rather than a general one (no new
    /// dependency for two variants and four fields): one line naming the
    /// kind, then either the embedded tree's hex oid, or one
    /// `url sha256 strip dest` line per component (`dest` last, so it may
    /// be empty without ambiguity).
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRecipe`] if the text does not match this shape.
    ///
    /// # Examples
    ///
    /// ```
    /// use ents_effect::Recipe;
    ///
    /// let text = "embedded 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n";
    /// let recipe = Recipe::parse(text).expect("parses");
    /// assert!(matches!(recipe, Recipe::Embedded { .. }));
    /// ```
    pub fn parse(text: &str) -> Result<Self> {
        let mut lines = text.lines().filter(|line| !line.trim().is_empty());
        let Some(first) = lines.next() else {
            return Err(invalid("empty recipe"));
        };
        let mut words = first.split_whitespace();
        match words.next() {
            Some(EMBEDDED) => {
                let hex = words
                    .next()
                    .ok_or_else(|| invalid("embedded recipe missing a tree oid"))?;
                let tree = ObjectId::from_hex(hex.as_bytes())
                    .map_err(|e| invalid(format!("invalid tree oid {hex:?}: {e}")))?;
                Ok(Self::Embedded { tree })
            }
            Some(DOWNLOADED) => {
                let mut components = Vec::new();
                for line in lines {
                    let mut fields = line.split_whitespace();
                    let url = fields
                        .next()
                        .ok_or_else(|| invalid("component line missing a url"))?
                        .to_owned();
                    let sha256 = fields
                        .next()
                        .ok_or_else(|| invalid("component line missing a sha256"))?
                        .to_owned();
                    let strip = fields
                        .next()
                        .ok_or_else(|| invalid("component line missing a strip count"))?
                        .parse::<u8>()
                        .map_err(|e| invalid(format!("invalid strip count: {e}")))?;
                    let dest = fields.next().unwrap_or("").to_owned();
                    components.push(Component {
                        url,
                        sha256,
                        strip,
                        dest,
                    });
                }
                if components.is_empty() {
                    return Err(invalid(
                        "a downloaded toolchain must list at least one component",
                    ));
                }
                Ok(Self::Downloaded { components })
            }
            Some(other) => Err(invalid(format!("unknown recipe kind {other:?}"))),
            None => Err(invalid("empty recipe")),
        }
    }

    /// Render this [`Recipe`] back into the text stored in
    /// [`ents_model::Toolchain::recipe`].
    ///
    /// # Examples
    ///
    /// ```
    /// use ents_effect::Recipe;
    ///
    /// let recipe = Recipe::Embedded {
    ///     tree: gix_hash::ObjectId::null(gix_hash::Kind::Sha1),
    /// };
    /// let text = recipe.render();
    /// assert_eq!(Recipe::parse(&text).expect("round-trips"), recipe);
    /// ```
    #[must_use]
    pub fn render(&self) -> String {
        match self {
            Self::Embedded { tree } => format!("{EMBEDDED} {tree}\n"),
            Self::Downloaded { components } => {
                let mut out = format!("{DOWNLOADED}\n");
                for c in components {
                    out.push_str(&format!("{} {} {} {}\n", c.url, c.sha256, c.strip, c.dest));
                }
                out
            }
        }
    }
}

/// Read the [`Toolchain`] entity named `name` from
/// `refs/meta/toolchains/<name>`, and parse its [`Toolchain::recipe`] as a
/// [`Recipe`].
///
/// # Errors
///
/// [`Error::UnknownToolchain`] when the ref does not exist or does not
/// resolve to a commit tree; [`Error::InvalidRecipe`] when its `recipe`
/// field does not parse.
///
/// # Examples
///
/// ```
/// use ents_effect::toolchain::resolve;
/// use ents_model::Toolchain;
/// use ents_testutil::{MemRefStore, ObjectStore, write_meta_entity};
///
/// let refs = MemRefStore::default();
/// let objects = ObjectStore::default();
/// let toolchain = Toolchain {
///     name: "rust-stable".into(),
///     recipe: "embedded 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n".into(),
/// };
/// let name: gix::refs::FullName = "refs/meta/toolchains/rust-stable".try_into().expect("valid");
/// write_meta_entity(&refs, &objects, name, &toolchain, None, 100);
///
/// let (entity, recipe) = resolve(&refs, &objects, "rust-stable").expect("resolves");
/// assert_eq!(entity.name, "rust-stable");
/// assert!(matches!(recipe, ents_effect::Recipe::Embedded { .. }));
/// ```
pub fn resolve(
    refs: &dyn RefStoreRead,
    objects: &impl Find,
    name: &str,
) -> Result<(Toolchain, Recipe)> {
    let refname = namespace::toolchain_ref(name)
        .map_err(|e| Error::UnknownToolchain(format!("{name}: {e}")))?;
    let Some(tip) = refs.get(refname.as_ref())? else {
        return Err(Error::UnknownToolchain(name.to_owned()));
    };
    let mut buf = Vec::new();
    let data = objects
        .try_find(&tip, &mut buf)
        .map_err(|source| Error::Decode {
            oid: tip,
            detail: source.to_string(),
        })?
        .ok_or(Error::Missing { oid: tip })?;
    if data.kind != Kind::Commit {
        return Err(Error::Decode {
            oid: tip,
            detail: "toolchain ref does not point at a commit".to_owned(),
        });
    }
    let commit = CommitRef::from_bytes(data.data, tip.kind()).map_err(|e| Error::Decode {
        oid: tip,
        detail: e.to_string(),
    })?;
    let toolchain: Toolchain = facet_git_tree::deserialize(&commit.tree(), objects)?;
    let recipe = Recipe::parse(&toolchain.recipe).map_err(|e| match e {
        Error::InvalidRecipe { detail, .. } => Error::InvalidRecipe {
            name: name.to_owned(),
            detail,
        },
        other => other,
    })?;
    Ok((toolchain, recipe))
}

fn invalid(detail: impl Into<String>) -> Error {
    Error::InvalidRecipe {
        name: String::new(),
        detail: detail.into(),
    }
}

/// A stable, filesystem-safe cache key for a [`Recipe`]: the embedded
/// tree's own hex oid, or each downloaded component's sha256 joined in
/// extraction order — the same bytes extracted differently (a different
/// `strip`/`dest`) are a different toolchain on disk, so those fields join
/// the key too.
#[must_use]
pub fn cache_key(recipe: &Recipe) -> String {
    match recipe {
        Recipe::Embedded { tree } => tree.to_string(),
        Recipe::Downloaded { components } => components
            .iter()
            .map(|c| format!("{}.{}.{}", c.sha256, c.strip, c.dest))
            .collect::<Vec<_>>()
            .join("-"),
    }
}

/// Resolve `recipe` to a host directory containing the toolchain's
/// activated `bin/`, extracted once under `cache_root` and reused on every
/// later call with the same recipe (`effect.toolchains`: "resolved during
/// effect execution").
///
/// # Errors
///
/// [`Error::Submodule`] or [`Error::NotUtf8`] for a tree this crate cannot
/// materialize; [`Error::Spawn`]/[`Error::Process`]/[`Error::HashMismatch`]
/// for a downloaded component that could not be fetched, verified, or
/// extracted; [`Error::Io`] for a host filesystem failure.
///
/// # Examples
///
/// An embedded recipe over the empty tree materializes an empty `bin/`.
///
/// ```
/// use ents_effect::Recipe;
/// use ents_effect::toolchain::materialize;
/// use ents_testutil::ObjectStore;
/// use gix_object::{Kind, Write as _};
///
/// let objects = ObjectStore::default();
/// let empty = objects.write_buf(Kind::Tree, b"").expect("write");
/// let recipe = Recipe::Embedded { tree: empty };
///
/// let dir = tempfile::tempdir().expect("tempdir");
/// let bin = materialize(&recipe, &objects, dir.path()).expect("materializes");
/// assert!(bin.ends_with("bin"));
/// assert!(bin.is_dir());
/// ```
pub fn materialize(recipe: &Recipe, objects: &impl Find, cache_root: &Path) -> Result<PathBuf> {
    let root = cache_root.join(cache_key(recipe));
    let bin = root.join("bin");
    if bin.is_dir() {
        return Ok(bin);
    }
    let tmp = cache_root.join(format!("{}.tmp", cache_key(recipe)));
    if tmp.exists() {
        remove_dir(&tmp)?;
    }
    make_dir(&tmp)?;

    match recipe {
        Recipe::Embedded { tree } => {
            let bin_tmp = tmp.join("bin");
            make_dir(&bin_tmp)?;
            crate::materialize::checkout(objects, *tree, &bin_tmp)?;
        }
        Recipe::Downloaded { components } => {
            make_dir(&tmp.join("bin"))?;
            for component in components {
                fetch_component(component, &tmp)?;
            }
        }
    }

    // Land atomically: a transient failure partway through must never leave
    // `bin` existing-but-incomplete, or the next call would trust a half
    // extraction forever.
    std::fs::rename(&tmp, &root).map_err(|source| Error::Io {
        path: root.clone(),
        source,
    })?;
    Ok(bin)
}

fn fetch_component(component: &Component, root: &Path) -> Result<()> {
    if component.dest.contains('/') || component.dest.contains("..") {
        return Err(Error::InvalidComponent(format!(
            "unsafe dest {:?}",
            component.dest
        )));
    }
    let dest = if component.dest.is_empty() {
        root.join("bin")
    } else {
        root.join("bin").join(&component.dest)
    };
    make_dir(&dest)?;

    let bytes = fetch(&component.url)?;
    let actual = sha256_hex(&bytes)?;
    if !actual.eq_ignore_ascii_case(&component.sha256) {
        return Err(Error::HashMismatch {
            url: component.url.clone(),
            expected: component.sha256.clone(),
            actual,
        });
    }

    let mut child = Command::new("tar")
        .args([
            "-x",
            "-C",
            dest.to_str().ok_or_else(|| Error::NotUtf8(dest.clone()))?,
            &format!("--strip-components={}", component.strip),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::Spawn {
            program: "tar".to_owned(),
            detail: e.to_string(),
        })?;
    {
        use std::io::Write as _;
        let mut stdin = child.stdin.take().ok_or_else(|| Error::Process {
            program: "tar".to_owned(),
            detail: "no stdin".to_owned(),
        })?;
        stdin.write_all(&bytes).map_err(|e| Error::Process {
            program: "tar".to_owned(),
            detail: e.to_string(),
        })?;
    }
    let output = child.wait_with_output().map_err(|e| Error::Process {
        program: "tar".to_owned(),
        detail: e.to_string(),
    })?;
    if !output.status.success() {
        return Err(Error::Process {
            program: "tar".to_owned(),
            detail: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok(())
}

/// `GET url`, via the system `curl` — shells out rather than adding an HTTP
/// dependency, the same rationale `pre-redo` used.
fn fetch(url: &str) -> Result<Vec<u8>> {
    let output = Command::new("curl")
        .args(["-sSL", "--fail", url])
        .output()
        .map_err(|e| Error::Spawn {
            program: "curl".to_owned(),
            detail: e.to_string(),
        })?;
    if !output.status.success() {
        return Err(Error::Process {
            program: "curl".to_owned(),
            detail: format!("could not fetch {url}"),
        });
    }
    Ok(output.stdout)
}

/// Hex-encoded sha256 of `bytes`, via the system `shasum` (macOS) or
/// `sha256sum` (Linux) — shells out rather than adding a hashing
/// dependency, the same rationale `pre-redo` used.
fn sha256_hex(bytes: &[u8]) -> Result<String> {
    let (program, args): (&str, &[&str]) = if Command::new("sha256sum")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        ("sha256sum", &[])
    } else {
        ("shasum", &["-a", "256"])
    };
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| Error::Spawn {
            program: program.to_owned(),
            detail: e.to_string(),
        })?;
    {
        use std::io::Write as _;
        let mut stdin = child.stdin.take().ok_or_else(|| Error::Process {
            program: program.to_owned(),
            detail: "no stdin".to_owned(),
        })?;
        stdin.write_all(bytes).map_err(|e| Error::Process {
            program: program.to_owned(),
            detail: e.to_string(),
        })?;
    }
    let mut out = String::new();
    child
        .stdout
        .take()
        .ok_or_else(|| Error::Process {
            program: program.to_owned(),
            detail: "no stdout".to_owned(),
        })?
        .read_to_string(&mut out)
        .map_err(|e| Error::Process {
            program: program.to_owned(),
            detail: e.to_string(),
        })?;
    let status = child.wait().map_err(|e| Error::Process {
        program: program.to_owned(),
        detail: e.to_string(),
    })?;
    if !status.success() {
        return Err(Error::Process {
            program: program.to_owned(),
            detail: "hashing failed".to_owned(),
        });
    }
    out.split_whitespace()
        .next()
        .map(str::to_owned)
        .ok_or_else(|| Error::Process {
            program: program.to_owned(),
            detail: "no hash in output".to_owned(),
        })
}

fn make_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path).map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })
}

fn remove_dir(path: &Path) -> Result<()> {
    std::fs::remove_dir_all(path).map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "unit test")]

    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::embedded(Recipe::Embedded { tree: ObjectId::null(gix_hash::Kind::Sha1) })]
    #[case::downloaded(Recipe::Downloaded {
        components: vec![
            Component { url: "https://example.test/a.tar.gz".into(), sha256: "a".repeat(64), strip: 2, dest: String::new() },
            Component { url: "https://example.test/b.tar.gz".into(), sha256: "b".repeat(64), strip: 1, dest: "bin".into() },
        ],
    })]
    // @relation(effect.toolchains, model.toolchain, scope=function, role=Verifies)
    fn recipe_round_trips_through_text(#[case] recipe: Recipe) {
        let text = recipe.render();
        assert_eq!(Recipe::parse(&text).expect("parses"), recipe);
    }

    #[rstest]
    #[case::empty("")]
    #[case::unknown_kind("frobnicated\n")]
    #[case::embedded_missing_oid("embedded\n")]
    #[case::downloaded_no_components("downloaded\n")]
    // @relation(effect.toolchains, scope=function, role=Verifies)
    fn parse_rejects_malformed_text(#[case] text: &str) {
        Recipe::parse(text).expect_err("malformed");
    }

    #[rstest]
    // @relation(effect.toolchains, scope=function, role=Verifies)
    fn cache_key_differs_by_extraction_shape() {
        let a = Recipe::Downloaded {
            components: vec![Component {
                url: "u".into(),
                sha256: "s".repeat(64),
                strip: 1,
                dest: String::new(),
            }],
        };
        let b = Recipe::Downloaded {
            components: vec![Component {
                url: "u".into(),
                sha256: "s".repeat(64),
                strip: 2,
                dest: String::new(),
            }],
        };
        assert_ne!(cache_key(&a), cache_key(&b));
    }
}
