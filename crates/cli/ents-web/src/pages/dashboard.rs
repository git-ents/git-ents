//! `GET /`: the repository overview -- a latest-commit freshness strip
//! above the rendered `README`, beside a sticky aside of a contents rail
//! (one live count per page family, which doubles as a smoke test that
//! every seam in [`crate::state::AppState`] actually reads) and a
//! language breakdown of the `HEAD` tree (`pre-redo:crates/git-ents-server/src/web/pages.rs`'s
//! `repo_page`, trimmed to the cards this single-repo, local crate has a
//! data surface for -- no clone URL, homepage, releases, or topics).
//!
//! The `README`, freshness-strip, and language reads browse the
//! repository's `HEAD` commit/tree through `gix`'s high-level
//! `Repository`/`Commit`/`Tree` types, opened fresh per request from
//! `state.path`, exactly as [`crate::pages::files`]/[`crate::pages::commits`]
//! do (and for the same reason: browsing arbitrary repository content is
//! not the `facet-git-tree` meta-ref convention the generic pages use).

use std::sync::Arc;

use axum::extract::State;
use gix::bstr::ByteSlice as _;
use gix_hash::ObjectId;
use gix_object::{Find, Write};
use maud::{Markup, html};

use crate::assets;
use crate::error::Result;
use crate::state::AppState;

/// A detected language's display name, swatch color (a literal CSS color,
/// since the pre-redo `--s-*` syntax palette those colors referenced was
/// not ported), and its share of the classified `HEAD` tree, as a
/// whole-number percentage.
type Lang = (&'static str, &'static str, u8);

/// `GET /`.
///
/// # Errors
///
/// Propagates a ref-store read failure.
pub async fn show<O>(State(state): State<Arc<AppState<O>>>) -> Result<maud::Markup>
where
    O: Find + Write + Send + 'static,
{
    let members = state.refs.iter_prefix("refs/meta/member/")?.count();
    let effects = state.refs.iter_prefix("refs/meta/effects/")?.count();
    let redactions = state.refs.iter_prefix("refs/meta/redactions/")?.count();
    let comments = state.refs.iter_prefix("refs/meta/comments/")?.count();
    let toolchains = state.refs.iter_prefix("refs/meta/toolchains/")?.count();

    let (main, langs) = repo_overview(&state);

    Ok(super::layout(
        &super::RepoHeader::from_state(&state),
        &super::identity_label(&state),
        super::Tab::Overview,
        "Overview",
        html! {
            div.overview {
                div { (main) }
                aside.aside {
                    div.card {
                        div.card-header { "contents" }
                        (contents_row("members", "/members", Some(members)))
                        (contents_row("account", "/account", None))
                        (contents_row("effects", "/effects", Some(effects)))
                        (contents_row("redactions", "/redactions", Some(redactions)))
                        (contents_row("toolchains", "/toolchains", Some(toolchains)))
                        (contents_row("comments", "/comments", Some(comments)))
                        (contents_row("inbox", "/inbox", None))
                    }
                    @if !langs.is_empty() {
                        div.card {
                            div.card-header { "languages" }
                            div.lang {
                                div.lang-bar {
                                    @for (_, color, pct) in &langs {
                                        span style={ "width:" (pct) "%;background:" (color) } {}
                                    }
                                }
                                ul.lang-legend {
                                    @for (lang, color, pct) in &langs {
                                        li {
                                            span.lang-dot style={ "background:" (color) } {}
                                            span { (lang) }
                                            span.pct { (pct) "%" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        },
    ))
}

/// One row of the contents card: a link to a page family, with its live
/// count when the family is one this crate counts.
fn contents_row(label: &str, href: &str, count: Option<usize>) -> Markup {
    html! {
        div.aside-row {
            a href=(href) { (label) }
            @if let Some(count) = count {
                span.count { (count) }
            }
        }
    }
}

/// The overview's main column and the language breakdown of its `HEAD`
/// tree: the rendered `README` when the root holds one, else a listing of
/// the root, else an empty-repository blankslate. Best-effort -- an
/// unopenable repository or an unborn `HEAD` degrades to the blankslate
/// with no languages, never an error (the page's contents card still
/// renders).
fn repo_overview<O>(state: &AppState<O>) -> (Markup, Vec<Lang>) {
    let Ok(repo) = gix::open(&state.path) else {
        return (blankslate(), Vec::new());
    };
    let Ok(tree) = repo.head_tree() else {
        return (blankslate(), Vec::new());
    };
    let langs = languages(&repo, &tree);
    let strip = freshness_strip(&repo);
    let content = if let Some((name, rendered)) = readme(&tree) {
        html! {
            div.card {
                div.card-header { (assets::icon_file()) (name) }
                div.doc-body { (rendered) }
            }
        }
    } else {
        let entries = root_entries(&tree);
        if entries.is_empty() {
            blankslate()
        } else {
            files_card(&entries)
        }
    };
    (html! { (strip) (content) }, langs)
}

/// The overview's latest-commit freshness strip, above the `README` card, a
/// single non-wrapping flex row: `HEAD`'s short oid linking to
/// `crate::pages::commits::show`, its subject (ellipsized on overflow, the
/// row's only flexible cell), the author and [`super::ago`] time (muted,
/// never wrapping), and a link into `crate::pages::commits::list`'s full
/// history. Renders nothing at all on an unborn `HEAD` or any other read
/// failure -- best-effort chrome, not a reason to fail the page.
fn freshness_strip(repo: &gix::Repository) -> Markup {
    let Ok(commit) = repo.head_commit() else {
        return html! {};
    };
    let Ok(message) = commit.message() else {
        return html! {};
    };
    let Ok(author) = commit.author() else {
        return html! {};
    };
    let seconds = author.time().map(|time| time.seconds).unwrap_or(0);
    let oid = commit.id().detach();
    html! {
        div.card.freshness {
            div.card-row {
                a href={ "/commit/" (oid) } { code { (super::short_oid(&oid)) } }
                span.freshness-subject { (message.title.to_str_lossy()) }
                span.freshness-meta { (author.name.to_str_lossy()) " \u{b7} " (super::ago(seconds)) }
                a.freshness-history href="/commits" { "history \u{2192}" }
            }
        }
    }
}

/// The empty-column placeholder ([`super::blankslate`]) shown when the
/// repository has no `README`, no readable root, or no `HEAD` at all.
fn blankslate() -> Markup {
    super::blankslate(
        "Nothing to show yet",
        html! { "Add a " code { "README" } " or browse the repository in " a href="/files" { "Files" } "." },
    )
}

/// The first root-tree blob whose stem is `README` and whose extension
/// this crate renders (Markdown or AsciiDoc), converted to HTML and paired
/// with its filename; `None` when there is none or it fails to render
/// (mirrors `pre-redo:.../pages.rs`'s `readme`).
fn readme(tree: &gix::Tree<'_>) -> Option<(String, Markup)> {
    let name = root_readme_name(tree)?;
    let entry = tree.lookup_entry_by_path(&name).ok()??;
    let blob = entry.object().ok()?.try_into_blob().ok()?;
    let text = String::from_utf8_lossy(&blob.data);
    render_doc(&name, &text).map(|rendered| (name, rendered))
}

/// The filename of the root's `README`, if it has a renderable one.
fn root_readme_name(tree: &gix::Tree<'_>) -> Option<String> {
    for entry in tree.iter() {
        let Ok(entry) = entry else { continue };
        if !entry.mode().is_blob() {
            continue;
        }
        let name = entry.filename().to_str_lossy();
        let is_readme = name
            .rsplit_once('.')
            .is_some_and(|(stem, _)| stem.eq_ignore_ascii_case("readme"));
        if is_readme && (crate::markdown::is_markdown(&name) || crate::asciidoc::is_asciidoc(&name))
        {
            return Some(name.into_owned());
        }
    }
    None
}

/// `text` rendered as its prose format (Markdown or AsciiDoc), or `None`
/// when it is neither or AsciiDoc rendering fails.
fn render_doc(name: &str, text: &str) -> Option<Markup> {
    if crate::markdown::is_markdown(name) {
        Some(crate::markdown::to_html(text))
    } else if crate::asciidoc::is_asciidoc(name) {
        crate::asciidoc::to_html(text).ok()
    } else {
        None
    }
}

/// The `(name, is_directory)` of each direct child of the root tree, in
/// tree order.
fn root_entries(tree: &gix::Tree<'_>) -> Vec<(String, bool)> {
    tree.iter()
        .filter_map(|entry| {
            let entry = entry.ok()?;
            Some((
                entry.filename().to_str_lossy().into_owned(),
                entry.mode().is_tree(),
            ))
        })
        .collect()
}

/// A root listing shown when there is no `README`: directories first, then
/// files, each linking into the Files browser.
fn files_card(entries: &[(String, bool)]) -> Markup {
    let mut entries = entries.to_vec();
    entries.sort_by(|(a_name, a_is_dir), (b_name, b_is_dir)| {
        b_is_dir.cmp(a_is_dir).then_with(|| a_name.cmp(b_name))
    });
    html! {
        div.card {
            div.card-header { "files" }
            @for (name, is_dir) in &entries {
                div.card-row.is-dir[*is_dir] {
                    a.row-link href={ "/files/" (name) } {
                        @if *is_dir { (assets::icon_folder()) } @else { (assets::icon_file()) }
                        (name)
                    }
                }
            }
        }
    }
}

/// The language breakdown of the whole `HEAD` tree: the top four languages
/// by total blob byte size, as `(name, color, percent)`, largest first --
/// byte-weighted like `pre-redo:.../git.rs`'s own `languages` (which shelled
/// out to `git ls-tree -l` for the sizes), except every size here comes
/// from [`gix::Repository::find_header`], an odb header lookup that never
/// reads a blob's full content. The shape and the top-four cap match
/// pre-redo's own.
fn languages(repo: &gix::Repository, tree: &gix::Tree<'_>) -> Vec<Lang> {
    let mut blobs = Vec::new();
    collect_blobs(repo, tree, &mut blobs);
    let mut totals: Vec<(&'static str, &'static str, u64)> = Vec::new();
    let mut grand: u64 = 0;
    for (name, oid) in &blobs {
        let Some((lang, color)) = classify(name) else {
            continue;
        };
        let size = repo
            .find_header(*oid)
            .map(|header| header.size())
            .unwrap_or(0);
        grand = grand.saturating_add(size);
        match totals.iter_mut().find(|(existing, _, _)| *existing == lang) {
            Some(entry) => entry.2 = entry.2.saturating_add(size),
            None => totals.push((lang, color, size)),
        }
    }
    if grand == 0 {
        return Vec::new();
    }
    totals.sort_by_key(|entry| std::cmp::Reverse(entry.2));
    totals.truncate(4);
    totals
        .into_iter()
        .map(|(lang, color, size)| {
            let pct = size.saturating_mul(100).checked_div(grand).unwrap_or(0);
            (lang, color, u8::try_from(pct).unwrap_or(100))
        })
        .filter(|(_, _, pct)| *pct > 0)
        .collect()
}

/// Recurse `tree`, pushing every blob's `(filename, oid)` onto `out`
/// -- [`languages`] weighs each by its odb header size, not by count.
/// Subtree reads that fail are skipped rather than propagated -- a
/// language bar is advisory chrome, not a reason to fail the whole page.
fn collect_blobs(repo: &gix::Repository, tree: &gix::Tree<'_>, out: &mut Vec<(String, ObjectId)>) {
    for entry in tree.iter() {
        let Ok(entry) = entry else { continue };
        if entry.mode().is_tree() {
            if let Ok(object) = repo.find_object(entry.oid().to_owned())
                && let Ok(subtree) = object.try_into_tree()
            {
                collect_blobs(repo, &subtree, out);
            }
        } else if entry.mode().is_blob() {
            out.push((
                entry.filename().to_str_lossy().into_owned(),
                entry.oid().to_owned(),
            ));
        }
    }
}

/// Map a filename to a language name and swatch color by its extension, or
/// `None` when the extension is not one this breakdown names (ported from
/// `pre-redo:.../git.rs`'s `classify_language`, its `var(--s-*)` colors
/// replaced with literals since that palette was not ported).
fn classify(name: &str) -> Option<(&'static str, &'static str)> {
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    let lang = match ext.as_str() {
        "rs" => ("Rust", "#dea584"),
        "html" | "htm" => ("HTML", "#e34c26"),
        "css" => ("CSS", "#563d7c"),
        "js" | "mjs" | "cjs" => ("JavaScript", "#f1e05a"),
        "ts" | "tsx" => ("TypeScript", "#3178c6"),
        "py" => ("Python", "#3572a5"),
        "go" => ("Go", "#00add8"),
        "c" | "h" => ("C", "#555555"),
        "cpp" | "cc" | "hpp" | "cxx" => ("C++", "#f34b7d"),
        "sh" | "bash" => ("Shell", "#89e051"),
        "toml" => ("TOML", "#9c4221"),
        "yaml" | "yml" => ("YAML", "#cb171e"),
        "json" => ("JSON", "#cbcb41"),
        "md" | "adoc" | "asciidoc" => ("Prose", "#a0a0a0"),
        _ => return None,
    };
    Some(lang)
}
