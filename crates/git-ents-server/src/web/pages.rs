//! The per-tab page renderers and their view helpers. Each top-level tab is its
//! own server-rendered route; with no client JavaScript beyond the
//! check-recording replay page, expanding a folder or opening a file is a
//! plain link back into these handlers.

use std::collections::HashSet;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use arborium::{Config, Highlighter, HtmlFormat};
use askama::Template;
use axum::response::{IntoResponse, Response};
use git_anchor::{LineRange, Projection};
use gix_date::Time;
use gix_hash::{ObjectId, Prefix};
use gix_object::bstr::ByteSlice;
use gix_object::tree::Entry;
use maud::{Markup, PreEscaped, html};

use super::git::{
    browse_path, git_output, git_output_bytes, git_output_capped, languages, latest_release,
    list_tree, parse_iso, releases, root_tree,
};
use super::icons::*;
use super::render::Render;
use super::{RepoMeta, Tab, component, not_found, repo_shell};

/// The largest blob or diff rendered in full. Past it a request would read an
/// unbounded object into memory and highlight it, so the view shows a truncation
/// notice instead — a cap on what one page can cost. 2 MiB comfortably covers
/// real source files while ruling out the multi-hundred-MiB objects that would
/// exhaust the server.
const MAX_RENDER_BYTES: usize = 2 * 1024 * 1024;

/// Render an Askama tab-body template into [`Markup`] the Maud page shell can
/// wrap. A template render failure is a programming error (a bad template),
/// surfaced as an inline notice rather than a panic.
fn render_body<T: Template>(tpl: &T) -> Markup {
    match tpl.render() {
        Ok(html) => PreEscaped(html),
        Err(err) => html! { div.card { div.card-row.muted { "Template error: " (err) } } },
    }
}

/// A single repository's overview: the rendered README beside an aside of
/// clone, about, releases, and language cards.
pub(super) async fn repo_page(repo: &Path, meta: &RepoMeta, host: Option<&str>) -> Markup {
    let rel = &meta.rel;
    let updated = git_output(repo, &["log", "-1", "--format=%ar"])
        .await
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    let is_empty = updated.is_none();
    let tree = root_tree(repo, !is_empty).await;
    let readme = readme(repo, &tree).await;
    let clone_url = clone_url(host, rel);
    let langs = languages(repo).await;
    let latest = latest_release(repo).await;
    let name = meta.name();

    let main = html! {
        @if is_empty {
            div.blankslate {
                h2 { "This repository is empty" }
                p { "Push a commit to get started." }
            }
        } @else if let Some((file, html)) = &readme {
            div.card {
                div.card-header { (icon_file()) " " (file) }
                article.adoc-body { (PreEscaped(html)) }
            }
        } @else if !tree.is_empty() {
            div.card {
                div.card-header { "Files" }
                @for entry in &tree {
                    @let name = entry.filename.to_str_lossy();
                    div.card-row.is-dir[entry.mode.is_tree()] {
                        @if entry.mode.is_tree() { (icon_folder()) } @else { (icon_file()) }
                        a href=(entry_href(rel, "", entry)) { (name.as_ref()) }
                    }
                }
            }
        }
    };

    let aside = html! {
        aside.aside {
            div.card {
                div.card-header { "Clone" }
                div.clone {
                    code { (clone_url) }
                    button.copy-btn data-copy={ "git clone " (clone_url) } { "Copy" }
                }
            }
            div.card {
                div.card-header { "About" }
                @if let Some(homepage) = &meta.homepage {
                    div.aside-row {
                        a href=(homepage) rel="noreferrer" { (homepage) }
                    }
                }
                @if let Some((lang, color, _)) = langs.first() {
                    div.aside-row {
                        span.dot style={ "background:" (color) } {}
                        span { (lang) }
                    }
                }
                @if let Some(updated) = &updated {
                    div.aside-row {
                        (icon_clock())
                        span.muted { "Updated " (updated) }
                    }
                }
            }
            @if let Some(release) = &latest {
                div.card {
                    div.card-header { "Releases" span.count { (meta.releases) } }
                    div.aside-row {
                        (icon_tag())
                        a href={ "/" (rel) "/releases" } { span.tag-pill { (release.tag) } }
                        span.badge-latest { "Latest" }
                    }
                    div.aside-row {
                        span.muted { (release.title) " · " (ago(&release.date)) }
                    }
                }
            }
            @if !langs.is_empty() {
                div.card {
                    div.card-header { "Languages" }
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
    };

    repo_shell(
        meta,
        Tab::Overview,
        name,
        html! { div.overview { div { (main) } (aside) } },
    )
}

/// The rendered README for the overview: the first AsciiDoc or Markdown file
/// in the root tree whose stem is `README`, converted to HTML, paired with its
/// filename. `None` when there is no such file or it fails to render.
async fn readme(repo: &Path, tree: &[Entry]) -> Option<(String, String)> {
    let entry = tree.iter().find(|e| {
        let name = e.filename.to_str_lossy();
        !e.mode.is_tree()
            && is_doc(&name)
            && name
                .rsplit_once('.')
                .is_some_and(|(stem, _)| stem.eq_ignore_ascii_case("readme"))
    })?;
    let name = entry.filename.to_str_lossy();
    let spec = format!("HEAD:{name}");
    let bytes = git_output_bytes(repo, &["cat-file", "-p", &spec]).await?;
    let html = doc_html(&name, &String::from_utf8_lossy(&bytes))?;
    Some((name.into_owned(), html))
}

/// The formatted-document HTML for `name`, when it is a prose format the forge
/// renders (AsciiDoc via acdc, Markdown via pulldown-cmark), or `None` when it
/// is not one or fails to render.
fn doc_html(name: &str, text: &str) -> Option<String> {
    match crate::render::mime_for_name(name) {
        "text/asciidoc" => crate::asciidoc::to_html(text),
        "text/markdown" => Some(crate::markdown::to_html(text)),
        _ => None,
    }
}

/// The clone URL for `rel`, using the request host when known.
fn clone_url(host: Option<&str>, rel: &str) -> String {
    match host {
        Some(host) => format!("http://{host}/{rel}"),
        None => format!("/{rel}"),
    }
}

/// The link to a tree entry: a `tree` view for directories, a `blob` view for
/// files. `dir` is the tree's path within the repo (empty at the root).
fn entry_href(rel: &str, dir: &str, entry: &Entry) -> String {
    let view = if entry.mode.is_tree() { "tree" } else { "blob" };
    let name = entry.filename.to_str_lossy();
    if dir.is_empty() {
        format!("/{rel}/{view}/{name}")
    } else {
        format!("/{rel}/{view}/{dir}/{name}")
    }
}

/// One row of the Files tree pane.
struct TreeRow {
    name: String,
    path: String,
    is_dir: bool,
    depth: usize,
    expanded: bool,
    selected: bool,
}

/// Walk the tree under `dir`, emitting a row per entry and recursing into any
/// directory in `expanded`. Boxed because the recursion is `async`.
fn collect_rows<'a>(
    repo: &'a Path,
    dir: &'a str,
    depth: usize,
    expanded: &'a HashSet<String>,
    selected: &'a str,
    out: &'a mut Vec<TreeRow>,
) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
    Box::pin(async move {
        let spec = if dir.is_empty() {
            "HEAD".to_owned()
        } else {
            format!("HEAD:{dir}")
        };
        for entry in list_tree(repo, &spec).await {
            let name = entry.filename.to_str_lossy();
            let is_dir = entry.mode.is_tree();
            let path = if dir.is_empty() {
                name.clone().into_owned()
            } else {
                format!("{dir}/{name}")
            };
            let is_expanded = is_dir && expanded.contains(&path);
            out.push(TreeRow {
                name: name.into_owned(),
                path: path.clone(),
                is_dir,
                depth,
                expanded: is_expanded,
                selected: !is_dir && path == selected,
            });
            if is_expanded {
                collect_rows(
                    repo,
                    &path,
                    depth.saturating_add(1),
                    expanded,
                    selected,
                    out,
                )
                .await;
            }
        }
    })
}

/// The Files tab: a tree pane beside a blob pane, both inside one card. With no
/// client JavaScript, expanding a folder or opening a file is a link to
/// `/<repo>/files/<path>`; the tree is rendered already expanded along the
/// selected path.
pub(super) async fn files_page(
    repo: &Path,
    meta: &RepoMeta,
    sub: &[&str],
    auth: Option<&super::Auth>,
    editing: bool,
) -> Response {
    let rel = &meta.rel;
    let Some(selected) = browse_path(sub) else {
        return not_found().into_response();
    };

    // Classify the selection so the right pane shows a file and the tree expands
    // the correct ancestors.
    let kind = if selected.is_empty() {
        None
    } else {
        git_output(repo, &["cat-file", "-t", &format!("HEAD:{selected}")])
            .await
            .map(|s| s.trim().to_owned())
    };
    let selected_file = matches!(kind.as_deref(), Some("blob")).then(|| selected.clone());
    let selected_dir = match kind.as_deref() {
        Some("tree") => selected.clone(),
        Some("blob") => selected
            .rsplit_once('/')
            .map_or(String::new(), |(d, _)| d.to_owned()),
        _ if selected.is_empty() => String::new(),
        _ => return not_found().into_response(),
    };

    // Expand every ancestor directory of the selection (and the selection itself
    // when it is a directory).
    let mut expanded = HashSet::new();
    let mut acc = String::new();
    for part in selected_dir.split('/').filter(|s| !s.is_empty()) {
        if !acc.is_empty() {
            acc.push('/');
        }
        acc.push_str(part);
        expanded.insert(acc.clone());
    }

    let mut rows = Vec::new();
    collect_rows(
        repo,
        "",
        0,
        &expanded,
        selected_file.as_deref().unwrap_or_default(),
        &mut rows,
    )
    .await;

    let right = match &selected_file {
        Some(path) => {
            let pane = blob_pane(repo, path).await;
            let comments = file_comments(repo, path).await;
            html! { (pane) (comments_card(&comments, comment_form(rel, path, auth, editing))) }
        }
        None => html! {
            div.files-empty {
                (icon_file())
                p { "Select a file to view its contents." }
            }
        },
    };

    let name = meta.name();
    repo_shell(
        meta,
        Tab::Files,
        name,
        html! {
            div.files {
                div.tree-pane {
                    div.tree-head { (icon_branch()) (meta.branch.as_deref().unwrap_or("HEAD")) }
                    @for row in &rows {
                        a.tree-row.sel[row.selected]
                            href={ "/" (rel) "/files/" (row.path) }
                            style={ "padding-left:" (row.depth.saturating_mul(15).saturating_add(8)) "px" }
                        {
                            @if row.is_dir {
                                span.chev.open[row.expanded] { (icon_chevron()) }
                                span.ic-folder { (icon_folder()) }
                            } @else {
                                span.chev {}
                                span.ic-file { (icon_file()) }
                            }
                            span { (row.name) }
                        }
                    }
                }
                div.blob-pane { (right) }
            }
        },
    )
    .into_response()
}

/// The right-hand pane of the Files view: a file's path, line/size meta, and its
/// syntax-highlighted source (or a binary notice).
async fn blob_pane(repo: &Path, path: &str) -> Markup {
    let Some((bytes, truncated)) = git_output_capped(
        repo,
        &["cat-file", "-p", &format!("HEAD:{path}")],
        MAX_RENDER_BYTES,
    )
    .await
    else {
        return html! { div.files-empty { "File not found." } };
    };
    let name = path.rsplit('/').next().unwrap_or(path);
    html! {
        div.blob-head {
            span { (path) }
            span.meta {
                @if !truncated && !is_binary(&bytes) {
                    span { (String::from_utf8_lossy(&bytes).lines().count()) " lines" }
                }
                span { (human_size(bytes.len())) @if truncated { "+" } }
                @if !truncated {
                    button.copy-btn data-copy=(String::from_utf8_lossy(&bytes)) { "Copy" }
                }
            }
        }
        @if truncated {
            div.binary { "File too large to display (over " (human_size(MAX_RENDER_BYTES)) ")." }
        } @else if is_binary(&bytes) {
            div.binary { "Binary file (" (human_size(bytes.len())) ") not shown." }
        } @else {
            (blob_body(name, &String::from_utf8_lossy(&bytes)))
        }
    }
}

/// A byte count rendered as a compact human-readable size.
fn human_size(bytes: usize) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    #[expect(
        clippy::cast_precision_loss,
        reason = "an approximate display size; exactness past 2^52 bytes is irrelevant"
    )]
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len().saturating_sub(1) {
        size /= 1024.0;
        unit = unit.saturating_add(1);
    }
    let label = UNITS.get(unit).unwrap_or(&"B");
    if unit == 0 {
        format!("{bytes} {label}")
    } else {
        format!("{size:.1} {label}")
    }
}

/// A commit id shortened to seven hex characters for display.
fn short_oid(oid: &ObjectId) -> String {
    Prefix::new(oid, 7)
        .ok()
        .map_or_else(|| oid.to_string(), |prefix| prefix.to_string())
}

/// A git date rendered as a relative "time ago" label, measured against the
/// current time.
fn ago(time: &Time) -> String {
    ago_seconds(time.seconds)
}

/// [`ago`] for a bare epoch-seconds timestamp.
fn ago_seconds(then: i64) -> String {
    let secs = Time::now_utc().seconds.saturating_sub(then).max(0);
    let mins = secs.checked_div(60).unwrap_or(0);
    let hours = mins.checked_div(60).unwrap_or(0);
    let days = hours.checked_div(24).unwrap_or(0);
    if mins == 0 {
        "just now".to_owned()
    } else if hours == 0 {
        plural(mins, "minute")
    } else if days == 0 {
        plural(hours, "hour")
    } else if days < 30 {
        plural(days, "day")
    } else if days < 365 {
        plural(days.checked_div(30).unwrap_or(0), "month")
    } else {
        plural(days.checked_div(365).unwrap_or(0), "year")
    }
}

/// Format `n` whole `unit`s with an "ago" suffix, pluralizing as needed.
fn plural(n: i64, unit: &str) -> String {
    if n == 1 {
        format!("1 {unit} ago")
    } else {
        format!("{n} {unit}s ago")
    }
}

/// A directory listing at `sub` within the repository.
pub(super) async fn tree_page(repo: &Path, meta: &RepoMeta, sub: &[&str]) -> Response {
    let rel = &meta.rel;
    let Some(dir) = browse_path(sub) else {
        return not_found().into_response();
    };
    let spec = if dir.is_empty() {
        "HEAD".to_owned()
    } else {
        format!("HEAD:{dir}")
    };
    let entries = list_tree(repo, &spec).await;
    if entries.is_empty() && !dir.is_empty() {
        return not_found().into_response();
    }
    let name = meta.name();
    repo_shell(
        meta,
        Tab::Files,
        name,
        html! {
            (crumbs(rel, &dir, false))
            div.card {
                div.card-header { "Files" }
                @if dir.is_empty() && entries.is_empty() {
                    div.card-row { "Empty repository." }
                }
                @for entry in &entries {
                    @let name = entry.filename.to_str_lossy();
                    div.card-row.is-dir[entry.mode.is_tree()] {
                        @if entry.mode.is_tree() { (icon_folder()) } @else { (icon_file()) }
                        a href=(entry_href(rel, &dir, entry)) { (name.as_ref()) }
                    }
                }
            }
        },
    )
    .into_response()
}

/// Which form of a blob the blob route shows: prose formats (AsciiDoc,
/// Markdown) rendered as a document, or the underlying source.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum BlobView {
    Rendered,
    Source,
}

/// A single file's contents at `sub`. A prose format renders as a formatted
/// document under [`BlobView::Rendered`] and as its source under
/// [`BlobView::Source`], with a toggle between the two; everything else is
/// syntax-highlighted source when the language is recognized and the file is
/// text.
pub(super) async fn blob_page(
    repo: &Path,
    meta: &RepoMeta,
    sub: &[&str],
    auth: Option<&super::Auth>,
    editing: bool,
    view: BlobView,
) -> Response {
    let rel = &meta.rel;
    let Some(path) = browse_path(sub).filter(|p| !p.is_empty()) else {
        return not_found().into_response();
    };
    let spec = format!("HEAD:{path}");
    if git_output(repo, &["cat-file", "-t", &spec])
        .await
        .as_deref()
        != Some("blob\n")
    {
        return not_found().into_response();
    }
    let Some((bytes, truncated)) =
        git_output_capped(repo, &["cat-file", "-p", &spec], MAX_RENDER_BYTES).await
    else {
        return not_found().into_response();
    };
    let name = path.rsplit('/').next().unwrap_or(&path);
    let displayable = !truncated && !is_binary(&bytes);
    let body = if truncated {
        html! { div.blob { div.binary { "File too large to display (over " (human_size(MAX_RENDER_BYTES)) ")." } } }
    } else if !displayable {
        html! { div.blob { div.binary { "Binary file (" (human_size(bytes.len())) ") not shown." } } }
    } else {
        let text = String::from_utf8_lossy(&bytes);
        match view {
            BlobView::Rendered => match doc_html(name, &text) {
                Some(html) => html! { div.card { article.adoc-body { (PreEscaped(html)) } } },
                None => blob_body(name, &text),
            },
            BlobView::Source => blob_body(name, &text),
        }
    };
    let comments = file_comments(repo, &path).await;
    repo_shell(
        meta,
        Tab::Files,
        name,
        html! {
            (crumbs(rel, &path, true))
            @if displayable && is_doc(name) {
                div.view-toggle {
                    @match view {
                        BlobView::Rendered => a.chip href={ "/" (rel) "/source/" (path) } { "View source" },
                        BlobView::Source => a.chip href={ "/" (rel) "/blob/" (path) } { "View rendered" },
                    }
                }
            }
            (body)
            (comments_card(&comments, comment_form(rel, &path, auth, editing)))
        },
    )
    .into_response()
}

/// Whether `name` is a prose format the forge renders as a document, and so
/// gets the rendered/source toggle.
fn is_doc(name: &str) -> bool {
    crate::render::mime_for_name(name) != "text/plain"
}

/// Render text file `source` with a line-number gutter — each number a
/// self-linking `#L<n>` anchor — highlighting via `arborium` when the filename
/// maps to a known grammar.
fn blob_body(name: &str, source: &str) -> Markup {
    let lines = source.lines().count().max(1);
    let highlighted = highlight(name, source);
    html! {
        div.blob {
            pre.blob-nums {
                @for n in 1..=lines {
                    a id={ "L" (n) } href={ "#L" (n) } { (n) }
                }
            }
            pre.blob-code {
                @match highlighted {
                    Some(html) => code.code { (PreEscaped(html)) },
                    None => code { (source) },
                }
            }
        }
    }
}

/// Highlighted HTML for `source`, or `None` when the filename has no grammar
/// (in which case the caller renders escaped plain text). The highlighter is
/// built and used synchronously so its non-`Send` grammar store is never held
/// across an `.await`.
fn highlight(name: &str, source: &str) -> Option<String> {
    let language = arborium::detect_language(name)?;
    let config = Config {
        html_format: HtmlFormat::ClassNames,
        ..Default::default()
    };
    Highlighter::with_config(config)
        .highlight(language, source)
        .ok()
}

/// Whether `bytes` looks like binary content (a NUL byte in the leading chunk,
/// the same heuristic git uses).
fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8000).any(|b| *b == 0)
}

/// A comment as a file view shows it: who wrote it and when, where its anchor
/// lands on `HEAD`, and its body, rendered as AsciiDoc (a comment carries no
/// filename to infer a MIME type from, so it gets the forge's default prose
/// treatment — see [`crate::render::DEFAULT_PROSE_MIME`]).
struct FileComment {
    author: String,
    seconds: i64,
    lines: Option<LineRange>,
    outdated: bool,
    body_html: String,
}

/// The comments whose anchors project onto `path` at `HEAD`, read off the
/// async runtime since git-comment reads the object database synchronously.
/// Comments that fail to project (say, an anchor commit the repository no
/// longer has) are skipped rather than failing the page.
async fn file_comments(repo: &Path, path: &str) -> Vec<FileComment> {
    let repo = repo.to_owned();
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        let Ok(comments) = git_comment::list(&repo) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (id, comment) in comments {
            let Ok(projection) = git_comment::project(&repo, &comment, "HEAD") else {
                continue;
            };
            let (landed, lines, outdated) = match projection {
                Projection::Current => (comment.anchor.path.clone(), comment.anchor.lines, false),
                Projection::Relocated { path, lines } => (path, lines, false),
                Projection::Outdated { path } => (path, None, true),
                Projection::FileDeleted => continue,
            };
            if landed != path {
                continue;
            }
            let provenance = git_comment::provenance(&repo, &id).ok().flatten();
            out.push(FileComment {
                author: provenance
                    .as_ref()
                    .map_or_else(|| "?".to_owned(), |p| p.created.name.clone()),
                seconds: provenance
                    .map_or(0, |p| i64::try_from(p.created.seconds).unwrap_or(i64::MAX)),
                lines,
                outdated,
                body_html: crate::render::to_html(crate::render::DEFAULT_PROSE_MIME, &comment.body),
            });
        }
        out
    })
    .await
    .unwrap_or_default()
}

/// The Comments card under a file view: existing comments, then the add form
/// when the viewer may comment; nothing when there are neither. A
/// line-anchored comment links its range to the gutter's `#L<n>` anchors; an
/// outdated one is flagged instead, since its lines no longer exist.
fn comments_card(comments: &[FileComment], form: Option<Markup>) -> Markup {
    if comments.is_empty() && form.is_none() {
        return html! {};
    }
    html! {
        div.card.file-comments {
            div.card-header { "Comments (" (comments.len()) ")" }
            @for comment in comments {
                div.comment-row {
                    div.comment-meta {
                        span.author { (comment.author) }
                        @if comment.seconds > 0 { span { (ago_seconds(comment.seconds)) } }
                        @if let Some(range) = comment.lines {
                            a.chip href={ "#L" (range.start) } {
                                @if range.start == range.end { "line " (range.start) }
                                @else { "lines " (range.start) "\u{2013}" (range.end) }
                            }
                        }
                        @if comment.outdated { span.chip { "outdated" } }
                    }
                    div.comment-body { (PreEscaped(&comment.body_html)) }
                }
            }
            @if let Some(form) = form { (form) }
        }
    }
}

/// The add-comment form under a file view, shown when a signed-in member views
/// a server that can land edits; `None` otherwise, since a submit would only
/// fail. The comment anchors to `HEAD`'s blob at `path`.
fn comment_form(
    rel: &str,
    path: &str,
    auth: Option<&super::Auth>,
    editing: bool,
) -> Option<Markup> {
    let auth = auth.filter(|a| editing && a.username.is_some())?;
    Some(html! {
        div.comment-row {
            form.edit-form method="post" action={ "/" (rel) "/comment" } {
                input type="hidden" name="csrf" value=(auth.csrf);
                input type="hidden" name="path" value=(path);
                label { "Lines" }
                input type="text" name="lines" placeholder="12 or 12:15 — empty for the whole file";
                label { "Comment" }
                textarea name="body" rows="3" placeholder="Anchored to this file as of the current HEAD" {}
                button.btn type="submit" { "Comment" }
            }
        }
    })
}

/// A single commit: its metadata and a colorized unified diff.
pub(super) async fn commit_page(repo: &Path, meta: &RepoMeta, sha: &str) -> Response {
    if sha.is_empty() || sha.len() > 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return not_found().into_response();
    }
    let Some(info) = git_output(
        repo,
        &["show", "-s", "--format=%H%x00%an%x00%aI%x00%s%x00%b", sha],
    )
    .await
    else {
        return not_found().into_response();
    };
    let mut parts = info.split('\u{0}');
    let Some(oid) = parts
        .next()
        .and_then(|h| ObjectId::from_hex(h.trim().as_bytes()).ok())
    else {
        return not_found().into_response();
    };
    let author = parts.next().unwrap_or_default().to_owned();
    let when = parts.next().and_then(parse_iso);
    let subject = parts.next().unwrap_or_default().to_owned();
    let body = parts.next().unwrap_or_default().trim_end().to_owned();
    let short = short_oid(&oid);
    let (patch_bytes, patch_truncated) = git_output_capped(
        repo,
        &["show", "--no-color", "--format=", "--patch", sha],
        MAX_RENDER_BYTES,
    )
    .await
    .unwrap_or_default();
    let patch = String::from_utf8_lossy(&patch_bytes);

    repo_shell(
        meta,
        Tab::Files,
        &subject,
        html! {
            div.card {
                div.card-header { (icon_commit()) " Commit " span.sha { (short) } }
                div.commit {
                    div.commit-subject { (subject) }
                    @if !body.is_empty() {
                        div.commit-msg { (body) }
                    }
                    div.commit-meta { (author) @if let Some(when) = &when { " · " (ago(when)) } }
                }
            }
            (diff_view(&patch))
            @if patch_truncated {
                div.card { div.binary { "Diff truncated (over " (human_size(MAX_RENDER_BYTES)) ")." } }
            }
        },
    )
    .into_response()
}

/// The Releases tab: tags presented as a changelog timeline, newest first.
pub(super) async fn releases_page(repo: &Path, meta: &RepoMeta) -> Markup {
    let releases = releases(repo).await;
    repo_shell(
        meta,
        Tab::Releases,
        "Releases",
        html! {
            div.page-header { h1.page-title { "Releases" } }
            @if releases.is_empty() {
                div.blankslate {
                    h2 { "No releases yet" }
                    p { "Push a tag to publish a release: " code { "git push <url> v1.0.0" } }
                }
            } @else {
                div.timeline {
                    @for (i, release) in releases.iter().enumerate() {
                        article.release.latest[i == 0] {
                            div.card {
                                div.release-head {
                                    (icon_tag())
                                    span.release-tag { (release.tag) }
                                    @if !release.title.is_empty() && release.title != release.tag {
                                        span.release-name { (release.title) }
                                    }
                                    @if i == 0 { span.badge-latest { "Latest" } }
                                    span.release-date { (ago(&release.date)) }
                                }
                                @if !release.body.is_empty() {
                                    div.release-body { p { (release.body) } }
                                }
                                div.release-foot {
                                    span.sha { (icon_commit()) (short_oid(&release.oid)) }
                                }
                            }
                        }
                    }
                }
            }
        },
    )
}

/// The Checks tab. The check set lives on `refs/meta/checks` (managed with
/// `git ents checks`); each push queues them and a worker runs them in a Sprite.
/// "Checks on HEAD" mirrors a GitHub PR checks list — one row per configured
/// check, its latest status against the current commit, linked to its recorded
/// terminal session when it has one; Recent runs and Configuration below it are
/// the full history and the raw set, as before.
pub(super) async fn checks_page(repo: &Path, meta: &RepoMeta) -> Markup {
    let rel = &meta.rel;
    let checks = component::load::<git_ents_core::checks::Check>(repo).await;
    let runs = load_runs(repo).await;
    let head = git_output(repo, &["rev-parse", "HEAD"])
        .await
        .map(|out| out.trim().to_owned())
        .filter(|head| !head.is_empty());
    let head_oid = head
        .as_deref()
        .and_then(|head| ObjectId::from_hex(head.as_bytes()).ok());
    let head_run = head_oid.and_then(|head_oid| {
        runs.as_ref()
            .ok()
            .and_then(|commits| commits.iter().find(|commit| commit.commit == head_oid))
            .and_then(|commit| commit.runs.first())
    });
    repo_shell(
        meta,
        Tab::Checks,
        "Checks",
        html! {
            div.page-header { h1.page-title { "Checks" } }
            p.shell-note {
                "Checks are configured on " code { "refs/meta/checks" }
                " (" code { "git ents checks list" } ") and run in a Sprite after each push; "
                "each run is recorded under " code { "refs/meta/runs/<commit>" } "."
            }
            div.card {
                div.card-header { "Checks on HEAD" }
                @match &checks {
                    Err(err) => div.card-row.muted { "Could not read checks: " (err) }
                    Ok(checks) if checks.is_empty() => {
                        div.card-row.muted {
                            "No checks configured on " code { "refs/meta/checks" } "."
                        }
                    }
                    Ok(checks) => {
                        @match head.as_deref() {
                            None => div.card-row.muted { "HEAD has no commits yet." }
                            Some(head) => {
                                @for check in checks {
                                    (head_check_row(rel, head, check, head_run))
                                }
                            }
                        }
                    }
                }
            }
            div.checks-grid {
                div.card {
                    div.card-header { "Recent runs" }
                    @match &runs {
                        Err(err) => div.card-row.muted { "Could not read runs: " (err) }
                        Ok(commits) if commits.is_empty() => div.card-row.muted { "No runs recorded yet." }
                        Ok(commits) => {
                            @for commit in commits.iter().take(25) {
                                @for run in &commit.runs {
                                    div.card-row.signer-row {
                                        code.key { (short_oid(&commit.commit)) }
                                        (run.render())
                                    }
                                }
                            }
                        }
                    }
                }
                (component::card(&checks))
            }
        },
    )
}

/// One check's row on the "Checks on HEAD" card: its name and its latest status
/// against `head`, linked to its recorded terminal session when `head_run`
/// carries one for it. A check with no outcome yet on `head` (just added, or
/// its run has not landed) reads "no run yet" rather than a stale result.
fn head_check_row(
    rel: &str,
    head: &str,
    check: &git_ents_core::checks::Check,
    head_run: Option<&git_ents_core::checks::Run>,
) -> Markup {
    let outcome =
        head_run.and_then(|run| run.results.iter().find(|result| result.name == check.name));
    let href = format!("/{rel}/checks/{head}/{}", check.name);
    html! {
        div.card-row.signer-row {
            code.key { (check.name) }
            (super::render::check_list_row(outcome, &href))
        }
    }
}

/// Find `name`'s outcome in `commit`'s latest recorded run, or `None` when
/// `commit` has no run, or no result under that name.
async fn latest_outcome(
    repo: &Path,
    commit_oid: ObjectId,
    name: &str,
) -> Option<git_ents_core::checks::RunOutcome> {
    load_runs(repo)
        .await
        .ok()?
        .into_iter()
        .find(|commit_runs| commit_runs.commit == commit_oid)
        .and_then(|commit_runs| commit_runs.runs.into_iter().next())
        .and_then(|run| run.results.into_iter().find(|result| result.name == name))
}

/// One check's terminal session on `commit` — reached by clicking a linked
/// status on the "Checks on HEAD" card. While the check is still `queued` or
/// `running` this is a live view, polling [`check_live_fragment`] until the
/// check settles; once it has, it replays the finished recording with
/// `asciinema-player`, or reports the exit code plain when there was no
/// output to replay. 404s when `commit` has no run recorded or `name` is not
/// among its results.
pub(super) async fn check_recording_page(
    repo: &Path,
    meta: &RepoMeta,
    commit: &str,
    name: &str,
    live_runs: &crate::checks::LiveRegistry,
) -> Response {
    let Some(commit_oid) = ObjectId::from_hex(commit.as_bytes()).ok() else {
        return not_found().into_response();
    };
    let Some(outcome) = latest_outcome(repo, commit_oid, name).await else {
        return not_found().into_response();
    };
    let short_commit = commit.get(..8).unwrap_or(commit);
    let rel = &meta.rel;

    let body = if super::render::is_in_progress(outcome.status) {
        let key = (repo.to_owned(), commit_oid, name.to_owned());
        let fragment_url = format!("/{rel}/checks/{commit}/{name}/live");
        let initial =
            super::render::live_fragment_body(crate::checks::live_snapshot(live_runs, &key));
        html! {
            p.shell-note {
                "This check is still " (outcome.status.to_string()) "; the view below updates live."
            }
            style { (PreEscaped(crate::asciidoc::TERMINAL_VIEW_CSS)) }
            div #live-terminal data-live-check=(fragment_url) { (initial) }
        }
    } else {
        let download_href = format!("/{rel}/checks/{commit}/{name}/download");
        super::render::check_result_view(&outcome, &download_href)
    };
    repo_shell(
        meta,
        Tab::Checks,
        &format!("{name} @ {short_commit}"),
        html! {
            div.page-header {
                h1.page-title { (name) " on " code { (short_commit) } }
            }
            (body)
        },
    )
    .into_response()
}

/// One poll of a running check's live output — the fragment [`LIVE_SCRIPT`]
/// swaps into the run page's `#live-terminal` container. Signals completion
/// (the check no longer has a live buffer: it settled, or was never queued)
/// via the `X-Check-Live: done` response header rather than the body, so the
/// script can tell a finished check apart from one that simply has no output
/// yet.
///
/// [`LIVE_SCRIPT`]: super::assets::LIVE_SCRIPT
pub(super) async fn check_live_fragment(
    repo: &Path,
    commit: &str,
    name: &str,
    live_runs: &crate::checks::LiveRegistry,
) -> Response {
    let Some(commit_oid) = ObjectId::from_hex(commit.as_bytes()).ok() else {
        return not_found().into_response();
    };
    let key = (repo.to_owned(), commit_oid, name.to_owned());
    let recording = crate::checks::live_snapshot(live_runs, &key);
    let done = recording.is_none();
    let body = super::render::live_fragment_body(recording).into_string();
    let header = if done { "done" } else { "running" };
    ([("x-check-live", header)], body).into_response()
}

/// Download a check's raw asciicast recording, for replaying outside the
/// browser (`asciinema play <file>`) or archiving. 404s under the same
/// conditions as [`check_recording_page`] (no run recorded, or none for
/// `name`), and also when the settled run has no recording to hand out.
pub(super) async fn check_recording_download(repo: &Path, commit: &str, name: &str) -> Response {
    let Some(commit_oid) = ObjectId::from_hex(commit.as_bytes()).ok() else {
        return not_found().into_response();
    };
    let Some(recording) = latest_outcome(repo, commit_oid, name)
        .await
        .and_then(|outcome| outcome.recording)
    else {
        return not_found().into_response();
    };
    let short_commit = commit.get(..8).unwrap_or(commit);
    let filename = format!(
        "{}-{}.cast",
        sanitize_filename(name),
        sanitize_filename(short_commit)
    );
    (
        [
            ("content-type", "application/x-asciicast".to_owned()),
            (
                "content-disposition",
                format!("attachment; filename=\"{filename}\""),
            ),
        ],
        recording,
    )
        .into_response()
}

/// Keep only characters safe for a `Content-Disposition` filename, so a check
/// name can't inject header syntax into the download response.
fn sanitize_filename(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Load the recorded runs off the async runtime, like [`component::load`].
async fn load_runs(repo: &Path) -> Result<Vec<git_ents_core::checks::CommitRuns>, String> {
    let repo = repo.to_owned();
    tokio::task::spawn_blocking(move || git_ents_core::checks::runs(&repo))
        .await
        .map_err(|err| err.to_string())?
        .map_err(|err| err.to_string())
}

/// The Issues ("Bug reports") tab: the real issue list from
/// `refs/meta/issues/<id>`, split into open and closed, with the filter chips
/// derived from the labels that exist. Issue creation is a write path that does
/// not exist yet, so the "New issue" button stays disabled.
pub(super) async fn issues_page(repo: &Path, meta: &RepoMeta) -> Markup {
    let tpl = match component::load::<git_ents_core::issues::Issue>(repo).await {
        Err(err) => IssuesTemplate {
            icons: Icons,
            error: Some(err),
            labels: Vec::new(),
            open: Vec::new(),
            open_count: 0,
            closed_count: 0,
        },
        Ok(issues) => {
            let open: Vec<&git_ents_core::issues::Issue> =
                issues.iter().filter(|issue| issue.is_open()).collect();
            let closed = issues.len().saturating_sub(open.len());
            let mut labels: Vec<String> = issues
                .iter()
                .flat_map(|issue| issue.labels.iter().cloned())
                .collect();
            labels.sort_unstable();
            labels.dedup();
            IssuesTemplate {
                icons: Icons,
                error: None,
                labels,
                open_count: open.len(),
                closed_count: closed,
                open: open
                    .iter()
                    .map(|issue| issue.render().into_string())
                    .collect(),
            }
        }
    };
    repo_shell(meta, Tab::Issues, "Bug reports", render_body(&tpl))
}

/// The Issues tab body: the open/closed filter and per-issue cards.
#[derive(Template)]
#[template(path = "issues.html")]
struct IssuesTemplate {
    icons: Icons,
    error: Option<String>,
    labels: Vec<String>,
    open: Vec<String>,
    open_count: usize,
    closed_count: usize,
}

/// The Settings tab: a projection over the repository's typed meta refs —
/// `refs/meta/config` (General), `refs/meta/members` (Members), and the derived
/// feature and check status. The General fields are editable in place by a
/// signed-in member when `editing` is set (the server has a signing key and the
/// gate); everything else is read-only.
pub(super) async fn settings_page(
    repo: &Path,
    meta: &RepoMeta,
    auth: Option<&super::Auth>,
    editing: bool,
) -> Markup {
    let members = component::load::<git_ents_core::members::Member>(repo).await;
    let checks = component::load::<git_ents_core::checks::Check>(repo).await;
    let config = load_repo_config(repo).await;
    repo_shell(
        meta,
        Tab::Settings,
        "Repository settings",
        html! {
            div.settings {
                div.page-header { h1.page-title { "Repository settings" } }
                p.shell-note {
                    "The repository's configuration on " code { "refs/meta/config" }
                    " and " code { "refs/meta/members" } "."
                }
                (settings_auth_banner(auth, editing))

                div.card {
                    div.card-header { "General" }
                    (setting_row("Repository name", meta.name()))
                    (setting_row("Default branch", meta.branch.as_deref().unwrap_or("—")))
                    (general_settings(meta, auth, editing))
                }

                div.card {
                    div.card-header { "Features" }
                    (feature_row("Bug reports", "Track and triage bugs.", meta.issues > 0))
                    (feature_row("Releases", "Publish tagged releases.", meta.releases > 0))
                    (feature_row("Checks (CI)", "Run signed CI records on push.", matches!(&checks, Ok(c) if !c.is_empty())))
                }

                p.shell-note {
                    "People on " code { "refs/meta/member/*" } " whose signed pushes are accepted "
                    "(" code { "git ents members list" } ")."
                }
                (component::card(&members))

                p.shell-note {
                    "Commands on " code { "refs/meta/checks" } " run against each push "
                    "(" code { "git ents checks list" } ")."
                }
                (component::card(&checks))

                div.card {
                    div.card-header { "Roles" }
                    p.shell-note {
                        "Ref-push gating by member role, on " code { "refs/meta/config" }
                        " — members join a role with " code { "git ents members add --role" } "."
                    }
                    @match &config {
                        Err(err) => div.card-row.muted { "Could not read config: " (err) }
                        Ok(config) if config.roles.is_empty() => {
                            div.card-row.muted {
                                "No roles configured — every member may push any ref."
                            }
                        }
                        Ok(config) => (config.render())
                    }
                }
            }
        },
    )
}

/// Load `refs/meta/config` off the async runtime, like [`component::load`].
async fn load_repo_config(repo: &Path) -> Result<git_ents_core::config::Config, String> {
    let repo = repo.to_owned();
    tokio::task::spawn_blocking(move || git_ents_core::config::load(&repo))
        .await
        .map_err(|err| err.to_string())?
        .map_err(|err| err.to_string())
}

/// The settings authorization banner: who is signed in and whether they may
/// edit this repository. When `editing` is unset the server cannot land edits at
/// all, so a member is told editing is disabled rather than offered controls
/// that would only fail.
fn settings_auth_banner(auth: Option<&super::Auth>, editing: bool) -> Markup {
    html! {
        @match auth {
            None => p.shell-note {
                a href="/login" { "Sign in" } " with a member web key to edit these settings."
            }
            Some(auth) if auth.username.is_some() && editing => p.shell-note.can-edit {
                "Signed in as " strong { (auth.label) } " — you can edit this repository."
            }
            Some(auth) if auth.username.is_some() => p.shell-note {
                "Signed in as " strong { (auth.label) } ", but this server has browser editing "
                "disabled, so settings are read-only."
            }
            Some(auth) => p.shell-note {
                "Signed in as " strong { (auth.label) } ", but this key is not a member of this "
                "repository, so settings are read-only."
            }
        }
    }
}

/// The editable General fields (description, homepage, topics): an edit form when
/// a signed-in member edits a server that can land edits, otherwise read-only rows.
fn general_settings(meta: &RepoMeta, auth: Option<&super::Auth>, editing: bool) -> Markup {
    let Some(auth) = auth.filter(|a| editing && a.username.is_some()) else {
        return html! {
            (setting_row("Description", meta.description.as_deref().unwrap_or("—")))
            (setting_row("Homepage", meta.homepage.as_deref().unwrap_or("—")))
            div.card-row {
                span.setting-label { "Topics" }
                @if meta.topics.is_empty() {
                    span.muted { "—" }
                } @else {
                    div.topics { @for topic in &meta.topics { span.topic { (topic) } } }
                }
            }
        };
    };
    let description = meta.description.as_deref().unwrap_or_default();
    let homepage = meta.homepage.as_deref().unwrap_or_default();
    let topics = meta.topics.join(", ");
    html! {
        div.card-row {
            form.edit-form method="post" action={ "/" (meta.rel) "/settings" } {
                input type="hidden" name="csrf" value=(auth.csrf);
                label { "Description" }
                input type="text" name="description" value=(description)
                    placeholder="A short description";
                label { "Homepage" }
                input type="text" name="homepage" value=(homepage)
                    placeholder="https://example.com";
                label { "Topics" }
                input type="text" name="topics" value=(topics)
                    placeholder="comma, separated, topics";
                button.btn type="submit" { "Save changes" }
            }
        }
    }
}

/// A read-only setting row: a label and its current value.
fn setting_row(label: &str, value: &str) -> Markup {
    html! {
        div.card-row {
            span.setting-label { (label) }
            span.muted { (value) }
        }
    }
}

/// A Features row showing the derived, read-only status of `title`: active when
/// the feature has backing data, empty otherwise.
fn feature_row(title: &str, desc: &str, on: bool) -> Markup {
    html! {
        div.feature-row {
            div {
                strong { (title) }
                p.muted { (desc) }
            }
            span.feature-status.on[on] { @if on { "Active" } @else { "Empty" } }
        }
    }
}

/// Render a unified diff, coloring each line by its leading marker.
fn diff_view(patch: &str) -> Markup {
    if patch.trim().is_empty() {
        return html! {};
    }
    html! {
        div.diff {
            @for line in patch.lines() {
                span class={ "ln " (diff_class(line)) } { (line) "\n" }
            }
        }
    }
}

/// The CSS class for a diff line, chosen from its leading marker.
fn diff_class(line: &str) -> &'static str {
    if line.starts_with("@@") {
        "hunk"
    } else if line.starts_with("+++") || line.starts_with("---") || line.starts_with("diff ") {
        "file"
    } else if line.starts_with("index ")
        || line.starts_with("new file")
        || line.starts_with("deleted file")
        || line.starts_with("old mode")
        || line.starts_with("new mode")
        || line.starts_with("rename ")
        || line.starts_with("similarity ")
        || line.starts_with("Binary files")
    {
        "meta"
    } else if line.starts_with('+') {
        "add"
    } else if line.starts_with('-') {
        "del"
    } else {
        "ctx"
    }
}

/// One segment of a breadcrumb trail: a label and, unless it is the current
/// file, the link to its directory listing.
struct Crumb {
    label: String,
    href: Option<String>,
}

/// Breadcrumb navigation from the repository root down through `path`. When
/// `is_file` is set, the final component is shown as plain text rather than a
/// link, since a file has no listing of its own.
fn crumbs(rel: &str, path: &str, is_file: bool) -> Markup {
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let mut acc = String::new();
    let mut trail: Vec<Crumb> = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        if !acc.is_empty() {
            acc.push('/');
        }
        acc.push_str(part);
        let is_last = i.saturating_add(1) == parts.len();
        let href = (!(is_last && is_file)).then(|| format!("/{rel}/tree/{acc}"));
        trail.push(Crumb {
            label: (*part).to_owned(),
            href,
        });
    }
    html! {
        nav.crumbs {
            a href={ "/" (rel) } { (rel) }
            @for crumb in &trail {
                span.sep { "/" }
                @match &crumb.href {
                    Some(href) => a href=(href) { (crumb.label) },
                    None => span.here { (crumb.label) },
                }
            }
        }
    }
}
