//! `GET /files`, `GET /files/{*path}`: a read-only directory listing and
//! blob viewer over the `HEAD` tree of the repository `git ents serve` is
//! serving. A `.md` blob renders via [`crate::markdown`], a
//! `.adoc`/`.asciidoc`/`.asc`/`.adc` blob via [`crate::asciidoc`], and
//! everything else as an escaped `<pre><code>` block -- no syntax
//! highlighting is ported (`pre-redo:crates/git-ents-server/src/web/pages.rs`'s
//! `arborium`-based `highlight` has no equivalent here; see
//! `crate::assets::OVERRIDES`'s own doc for the rest of what pre-redo
//! carried that this crate does not).
//!
//! Tree/blob reads go through `gix`'s high-level `Repository`/`Tree`/`Blob`
//! types (`repo.head_tree()`, `Tree::lookup_entry_by_path`,
//! `Entry::object`), opened fresh per request from `state.path` -- the
//! same `gix::open(repo_path)` pattern `ents_forge::comment::add`/`show`
//! already use to browse a live working tree, not the
//! `facet-git-tree`/`gix_object::Find` convention the rest of this crate's
//! pages use to read typed meta-ref entities (`facet-git-tree` is for
//! structured meta-ref data; browsing arbitrary repository content is not
//! that).

use std::sync::Arc;

use axum::extract::{Path, State};
use gix::bstr::ByteSlice as _;
use gix_object::{Find, Write};
use maud::{Markup, html};

use crate::assets;
use crate::error::{Error, Result};
use crate::state::AppState;

/// `GET /files`: the repository root directory listing.
///
/// # Errors
///
/// Propagates a `gix::open`/tree-read failure.
pub async fn root<O>(State(state): State<Arc<AppState<O>>>) -> Result<Markup>
where
    O: Find + Write + Send + 'static,
{
    at(&state, "")
}

/// `GET /files/{*path}`: a directory listing or blob view at `path`.
///
/// # Errors
///
/// [`Error::NotFound`] if `path` does not name a tree or blob entry (or
/// contains a `.`/`..` component); otherwise propagates a
/// `gix::open`/tree-read failure.
pub async fn show<O>(
    State(state): State<Arc<AppState<O>>>,
    Path(path): Path<String>,
) -> Result<Markup>
where
    O: Find + Write + Send + 'static,
{
    at(&state, &path)
}

/// The shared implementation behind [`root`] and [`show`]: resolve `path`
/// against `HEAD`'s tree and render whichever of a directory listing or a
/// blob view it names.
fn at<O>(state: &AppState<O>, path: &str) -> Result<Markup> {
    if !is_safe_path(path) {
        return Err(Error::NotFound {
            what: path.to_owned(),
        });
    }

    let repo = gix::open(&state.path).map_err(|source| Error::Repo(source.to_string()))?;
    let head_tree = match repo.head_tree() {
        Ok(tree) => tree,
        // An unborn HEAD (a freshly initialized, still-empty repository)
        // reads as an empty root directory, not a failure -- mirrors
        // `pre-redo:crates/git-ents-server/src/web/git.rs`'s `root_tree`,
        // which returned an empty entry list rather than erroring when the
        // repository had no `HEAD` yet.
        Err(_) if path.is_empty() => {
            return Ok(super::layout(
                &super::RepoHeader::from_state(state),
                super::Tab::Files,
                "files",
                html! {
                    (crumbs(path))
                    (dir_listing(path, Vec::new()))
                },
            ));
        }
        Err(_) => {
            return Err(Error::NotFound {
                what: path.to_owned(),
            });
        }
    };

    if path.is_empty() {
        let entries = tree_entries(&head_tree)?;
        return Ok(super::layout(
            &super::RepoHeader::from_state(state),
            super::Tab::Files,
            "files",
            html! {
                (crumbs(path))
                (dir_listing(path, entries))
            },
        ));
    }

    let entry = head_tree
        .lookup_entry_by_path(path)
        .map_err(|source| Error::Repo(source.to_string()))?
        .ok_or_else(|| Error::NotFound {
            what: path.to_owned(),
        })?;

    if entry.mode().is_tree() {
        let subtree = entry
            .object()
            .map_err(|source| Error::Repo(source.to_string()))?
            .try_into_tree()
            .map_err(|source| Error::Repo(source.to_string()))?;
        let entries = tree_entries(&subtree)?;
        Ok(super::layout(
            &super::RepoHeader::from_state(state),
            super::Tab::Files,
            path,
            html! {
                (crumbs(path))
                (dir_listing(path, entries))
            },
        ))
    } else if entry.mode().is_blob() {
        let blob = entry
            .object()
            .map_err(|source| Error::Repo(source.to_string()))?
            .try_into_blob()
            .map_err(|source| Error::Repo(source.to_string()))?;
        let name = path.rsplit('/').next().unwrap_or(path);
        Ok(super::layout(
            &super::RepoHeader::from_state(state),
            super::Tab::Files,
            path,
            html! {
                (crumbs(path))
                (blob_view(name, &blob.data)?)
            },
        ))
    } else {
        // A symlink or a submodule (gitlink) -- neither is a tree or a
        // blob this browser can render.
        Err(Error::NotFound {
            what: path.to_owned(),
        })
    }
}

/// Whether `path` is safe to resolve against a tree: no empty, `.`, or
/// `..` component. The empty root path is itself safe.
fn is_safe_path(path: &str) -> bool {
    path.is_empty()
        || path
            .split('/')
            .all(|s| !s.is_empty() && s != "." && s != "..")
}

/// One `(name, is_directory)` pair per direct child of `tree`, in tree
/// order (not yet sorted -- [`dir_listing`] sorts for display).
fn tree_entries(tree: &gix::Tree<'_>) -> Result<Vec<(String, bool)>> {
    tree.iter()
        .map(|entry| {
            let entry = entry.map_err(|source| Error::Repo(source.to_string()))?;
            Ok((
                entry.filename().to_str_lossy().into_owned(),
                entry.mode().is_tree(),
            ))
        })
        .collect()
}

/// The link to a child of the directory at `dir` (empty at the root).
fn child_href(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        format!("/files/{name}")
    } else {
        format!("/files/{dir}/{name}")
    }
}

/// A directory listing at `dir`: entries sorted directories-first then
/// alphabetically, each an icon and a link one level deeper.
fn dir_listing(dir: &str, mut entries: Vec<(String, bool)>) -> Markup {
    entries.sort_by(|(a_name, a_is_dir), (b_name, b_is_dir)| {
        b_is_dir.cmp(a_is_dir).then_with(|| a_name.cmp(b_name))
    });
    html! {
        div.card {
            div.card-header { "files" }
            @if entries.is_empty() {
                div.card-row.muted { "Empty directory." }
            }
            @for (name, is_dir) in &entries {
                div.card-row.is-dir[*is_dir] {
                    a.row-link href=(child_href(dir, name)) {
                        @if *is_dir { (assets::icon_folder()) } @else { (assets::icon_file()) }
                        (name)
                    }
                }
            }
        }
    }
}

/// Breadcrumb navigation from the repository's files root down through
/// `path`, `chevron-right` icons separating segments.
fn crumbs(path: &str) -> Markup {
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let mut acc = String::new();
    let mut trail: Vec<(String, Option<String>)> =
        vec![("files".to_owned(), Some("/files".to_owned()))];
    for (index, part) in parts.iter().enumerate() {
        if !acc.is_empty() {
            acc.push('/');
        }
        acc.push_str(part);
        let is_last = index.saturating_add(1) == parts.len();
        let href = (!is_last).then(|| format!("/files/{acc}"));
        trail.push(((*part).to_owned(), href));
    }
    html! {
        nav.crumbs {
            @for (index, (label, href)) in trail.iter().enumerate() {
                @if index > 0 { span.sep { (assets::icon_chevron()) } }
                @match href {
                    Some(href) => a href=(href) { (label) },
                    None => span.here { (label) },
                }
            }
        }
    }
}

/// Whether `bytes` looks like binary content (a NUL byte in the leading
/// chunk -- the same heuristic git itself uses, carried over from
/// `pre-redo:crates/git-ents-server/src/web/pages.rs`'s own `is_binary`).
fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8000).any(|b| *b == 0)
}

/// A single blob's contents: a Markdown/AsciiDoc document rendered as such
/// via [`crate::markdown`]/[`crate::asciidoc`], a binary-content
/// placeholder, or a line-numbered, escaped source view of the raw text.
///
/// The source view mirrors `pre-redo:crates/git-ents-server/src/web/pages.rs`'s
/// `blob_body`: a `.blob` grid pairing a `pre.blob-nums` gutter of
/// per-line `#L{n}` anchors with the escaped `pre.blob-code` code column
/// (no syntax highlighting is ported, so the code stays a plain escaped
/// `<code>` rather than pre-redo's highlighted spans).
///
/// # Errors
///
/// Propagates [`crate::asciidoc::to_html`]'s own [`Error::Asciidoc`].
fn blob_view(name: &str, bytes: &[u8]) -> Result<Markup> {
    if is_binary(bytes) {
        return Ok(html! { div.binary { "Binary file (" (bytes.len()) " bytes) not shown." } });
    }
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Ok(html! { div.binary { "Binary file (" (bytes.len()) " bytes) not shown." } });
    };
    if crate::markdown::is_markdown(name) {
        return Ok(html! { div.card { div.doc-body { (crate::markdown::to_html(text)) } } });
    }
    if crate::asciidoc::is_asciidoc(name) {
        return Ok(html! { div.card { div.doc-body { (crate::asciidoc::to_html(text)?) } } });
    }
    let lines = text.lines().count().max(1);
    Ok(html! {
        div.blob {
            pre.blob-nums {
                @for n in 1..=lines {
                    a id={ "L" (n) } href={ "#L" (n) } { (n) }
                }
            }
            pre.blob-code {
                code { (text) }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "unit test")]

    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::empty("", true)]
    #[case::simple("src/main.rs", true)]
    #[case::nested("a/b/c", true)]
    #[case::dot(".", false)]
    #[case::dotdot("..", false)]
    #[case::traversal("a/../b", false)]
    #[case::trailing_slash("a/", false)]
    #[case::double_slash("a//b", false)]
    fn is_safe_path_rejects_dot_components_and_empty_segments(
        #[case] path: &str,
        #[case] expected: bool,
    ) {
        assert_eq!(is_safe_path(path), expected);
    }

    #[test]
    fn dir_listing_sorts_directories_first_then_alphabetically() {
        let entries = vec![
            ("zeta.txt".to_owned(), false),
            ("alpha".to_owned(), true),
            ("beta.txt".to_owned(), false),
            ("gamma".to_owned(), true),
        ];
        let rendered = dir_listing("", entries).into_string();
        let alpha = rendered.find("alpha").expect("alpha listed");
        let gamma = rendered.find("gamma").expect("gamma listed");
        let beta = rendered.find("beta.txt").expect("beta listed");
        let zeta = rendered.find("zeta.txt").expect("zeta listed");
        assert!(alpha < gamma, "directories sort among themselves");
        assert!(gamma < beta, "every directory sorts before every file");
        assert!(beta < zeta, "files sort among themselves");
    }

    #[test]
    fn blob_view_renders_markdown_as_a_heading_not_raw_markup() {
        let rendered = blob_view("readme.md", b"# Title\n")
            .expect("markdown renders")
            .into_string();
        assert!(rendered.contains("<h1>Title</h1>"));
    }

    #[test]
    fn blob_view_renders_asciidoc_as_a_heading_not_raw_markup() {
        let rendered = blob_view("readme.adoc", b"= Title\n\nBody.\n")
            .expect("asciidoc renders")
            .into_string();
        assert!(rendered.contains("<h1>Title</h1>"));
    }

    #[test]
    fn blob_view_escapes_plain_text_into_a_line_numbered_code_block() {
        let rendered = blob_view("main.rs", b"fn main() { let x = 1 < 2; }")
            .expect("plain text renders")
            .into_string();
        assert!(rendered.contains("blob-nums"));
        assert!(rendered.contains("<pre class=\"blob-code\"><code>"));
        assert!(rendered.contains("1 &lt; 2"));
    }

    #[test]
    fn blob_view_shows_a_placeholder_for_binary_content() {
        let rendered = blob_view("data.bin", b"\0\x01\x02binary")
            .expect("binary placeholder renders")
            .into_string();
        assert!(rendered.contains("Binary file"));
    }

    #[test]
    fn child_href_nests_under_the_current_directory() {
        assert_eq!(child_href("", "src"), "/files/src");
        assert_eq!(child_href("src", "main.rs"), "/files/src/main.rs");
    }
}
