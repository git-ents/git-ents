//! The per-tab page renderers and their view helpers. Each top-level tab is its
//! own server-rendered route; with no client JavaScript, expanding a folder or
//! opening a file is a plain link back into these handlers.

use std::collections::HashSet;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use arborium::{Config, Highlighter, HtmlFormat};
use axum::response::{IntoResponse, Response};
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
use super::{RepoMeta, Tab, not_found, repo_shell};

/// The largest blob or diff rendered in full. Past it a request would read an
/// unbounded object into memory and highlight it, so the view shows a truncation
/// notice instead — a cap on what one page can cost. 2 MiB comfortably covers
/// real source files while ruling out the multi-hundred-MiB objects that would
/// exhaust the server.
const MAX_RENDER_BYTES: usize = 2 * 1024 * 1024;

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

/// The rendered README for the overview: the first AsciiDoc file in the root
/// tree whose stem is `README`, converted to HTML, paired with its filename.
/// `None` when there is no such file or it fails to render.
async fn readme(repo: &Path, tree: &[Entry]) -> Option<(String, String)> {
    let entry = tree.iter().find(|e| {
        let name = e.filename.to_str_lossy();
        !e.mode.is_tree()
            && crate::asciidoc::is_asciidoc(&name)
            && name
                .rsplit_once('.')
                .is_some_and(|(stem, _)| stem.eq_ignore_ascii_case("readme"))
    })?;
    let name = entry.filename.to_str_lossy();
    let spec = format!("HEAD:{name}");
    let bytes = git_output_bytes(repo, &["cat-file", "-p", &spec]).await?;
    let html = crate::asciidoc::to_html(&String::from_utf8_lossy(&bytes))?;
    Some((name.into_owned(), html))
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
pub(super) async fn files_page(repo: &Path, meta: &RepoMeta, sub: &[&str]) -> Response {
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
        Some(path) => blob_pane(repo, path).await,
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
    let secs = Time::now_utc().seconds.saturating_sub(time.seconds).max(0);
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

/// A single file's contents at `sub`, syntax-highlighted when the language is
/// recognized and the file is text.
pub(super) async fn blob_page(repo: &Path, meta: &RepoMeta, sub: &[&str]) -> Response {
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
    let body = if truncated {
        html! { div.blob { div.binary { "File too large to display (over " (human_size(MAX_RENDER_BYTES)) ")." } } }
    } else if is_binary(&bytes) {
        html! { div.blob { div.binary { "Binary file (" (human_size(bytes.len())) ") not shown." } } }
    } else {
        let text = String::from_utf8_lossy(&bytes);
        match crate::asciidoc::is_asciidoc(name)
            .then(|| crate::asciidoc::to_html(&text))
            .flatten()
        {
            Some(html) => html! { div.card { article.adoc-body { (PreEscaped(html)) } } },
            None => blob_body(name, &text),
        }
    };
    repo_shell(
        meta,
        Tab::Files,
        name,
        html! {
            (crumbs(rel, &path, true))
            (body)
        },
    )
    .into_response()
}

/// Render text file `source` with a line-number gutter, highlighting via
/// `arborium` when the filename maps to a known grammar.
fn blob_body(name: &str, source: &str) -> Markup {
    let lines = source.lines().count().max(1);
    let mut gutter = String::new();
    for n in 1..=lines {
        gutter.push_str(&n.to_string());
        gutter.push('\n');
    }
    let highlighted = highlight(name, source);
    html! {
        div.blob {
            pre.blob-nums { (gutter) }
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
/// The Configuration card reflects the live set; Recent runs reflects the run
/// log on `refs/meta/runs`, including in-flight `queued`/`running` runs.
pub(super) async fn checks_page(repo: &Path, meta: &RepoMeta) -> Markup {
    let checks = load_checks(repo).await;
    let runs = load_runs(repo).await;
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
                                        code.key { (commit.commit.get(..8).unwrap_or(&commit.commit)) }
                                        (run.render())
                                    }
                                }
                            }
                        }
                    }
                }
                div.card {
                    div.card-header {
                        "Configuration"
                        @if let Ok(checks) = &checks { span.count { (checks.len()) } }
                    }
                    @match &checks {
                        Err(err) => div.card-row.muted { "Could not read checks: " (err) }
                        Ok(checks) if checks.is_empty() => {
                            div.card-row.muted {
                                "No checks configured on " code { "refs/meta/checks" } "."
                            }
                        }
                        Ok(checks) => {
                            @for check in checks {
                                (check.render())
                            }
                        }
                    }
                }
            }
        },
    )
}

/// Load the configured check set off the async runtime, since `checks::load`
/// shells out to git and reads the object database synchronously.
async fn load_checks(repo: &Path) -> Result<Vec<git_ents::checks::Check>, String> {
    let repo = repo.to_owned();
    tokio::task::spawn_blocking(move || git_ents::checks::load(&repo))
        .await
        .map_err(|err| err.to_string())?
        .map_err(|err| err.to_string())
}

/// Load the recorded runs off the async runtime, like [`load_checks`].
async fn load_runs(repo: &Path) -> Result<Vec<git_ents::checks::CommitRuns>, String> {
    let repo = repo.to_owned();
    tokio::task::spawn_blocking(move || git_ents::checks::runs(&repo))
        .await
        .map_err(|err| err.to_string())?
        .map_err(|err| err.to_string())
}

/// The Issues ("Bug reports") tab: the real issue list from
/// `refs/meta/issues/<id>`, split into open and closed, with the filter chips
/// derived from the labels that exist. Issue creation is a write path that does
/// not exist yet, so the "New issue" button stays disabled.
pub(super) async fn issues_page(repo: &Path, meta: &RepoMeta) -> Markup {
    let issues = load_issues(repo).await;
    let body = match &issues {
        Err(err) => html! { div.card { div.card-row.muted { "Could not read issues: " (err) } } },
        Ok(issues) => {
            let open: Vec<&(String, git_ents::issues::Issue)> =
                issues.iter().filter(|(_id, i)| i.is_open()).collect();
            let closed = issues.len().saturating_sub(open.len());
            let mut labels: Vec<&str> = issues
                .iter()
                .flat_map(|(_id, i)| i.labels.iter().map(String::as_str))
                .collect();
            labels.sort_unstable();
            labels.dedup();
            html! {
                div.filter-row {
                    div.filter-search {
                        (icon_search())
                        input type="search" placeholder="Filter bug reports" aria-label="Filter" disabled;
                    }
                    span.chip.active { "All" }
                    @for label in &labels {
                        span.chip { (label) }
                    }
                }
                div.card {
                    div.card-header.subtabs {
                        span.subtab.active { (icon_issue()) "Open" span.tab-count { (open.len()) } }
                        span.subtab { (icon_check()) "Closed" span.tab-count { (closed) } }
                    }
                    @if open.is_empty() {
                        div.blankslate {
                            h2 { "No open bug reports" }
                            p { "Open one to start tracking a bug." }
                        }
                    } @else {
                        @for (_id, issue) in &open {
                            (issue.render())
                        }
                    }
                }
            }
        }
    };
    repo_shell(
        meta,
        Tab::Issues,
        "Bug reports",
        html! {
            div.issues-head {
                h1.page-title { "Bug reports" }
                button.btn-primary type="button" disabled title="Not available yet" { (icon_plus()) "New issue" }
            }
            (body)
        },
    )
}

/// Load the repository's issues off the async runtime, since `issues::list`
/// reads the object database synchronously.
async fn load_issues(repo: &Path) -> Result<Vec<(String, git_ents::issues::Issue)>, String> {
    let repo = repo.to_owned();
    tokio::task::spawn_blocking(move || git_ents::issues::list(&repo))
        .await
        .map_err(|err| err.to_string())?
        .map_err(|err| err.to_string())
}

/// The Settings tab. Persisting changes needs a config store that does not
/// exist yet, so the controls reflect the repository's current real values and
/// are presented read-only.
pub(super) async fn settings_page(repo: &Path, meta: &RepoMeta) -> Markup {
    let name = meta.name();
    let signers = load_signers(repo).await;
    repo_shell(
        meta,
        Tab::Settings,
        "Repository settings",
        html! {
            div.settings {
                div.page-header { h1.page-title { "Repository settings" } }
                p.shell-note { "These reflect the repository's current configuration." }

                div.card {
                    div.card-header { "General" }
                    div.field {
                        label { "Repository name" }
                        input type="text" value=(name) disabled title="Not editable yet";
                    }
                    div.field {
                        label { "Description" }
                        textarea rows="2" disabled title="Not editable yet" { (meta.description.as_deref().unwrap_or_default()) }
                    }
                    div.field {
                        label { "Default branch" }
                        input type="text" value=(meta.branch.as_deref().unwrap_or("—")) disabled title="Not editable yet";
                    }
                }

                div.card {
                    div.card-header { "Features" }
                    (feature_row("Bug reports", "Track and triage bugs.", meta.issues > 0))
                    (feature_row("Releases", "Publish tagged releases.", meta.releases > 0))
                    (feature_row("Checks (CI)", "Run signed CI records on push.", false))
                    (feature_row("Wiki", "A separate documentation space.", false))
                }

                div.card {
                    div.card-header {
                        "Members"
                        @if let Ok(signers) = &signers { span.count { (signers.len()) } }
                    }
                    p.shell-note {
                        "Keys on " code { "refs/meta/members" } " whose signed pushes are accepted "
                        "(" code { "git ents members list" } ")."
                    }
                    @match &signers {
                        Err(err) => div.card-row.muted { "Could not read members: " (err) }
                        Ok(signers) if signers.is_empty() => {
                            div.card-row.muted {
                                "No members — pushes are open until the first key is added."
                            }
                        }
                        Ok(signers) => {
                            @for signer in signers {
                                (signer.render())
                            }
                        }
                    }
                }

                div.card {
                    div.card-header { "Visibility" }
                    (visibility_row("Public", "Anyone can read this repository.", true))
                    (visibility_row("Private", "Only collaborators can read it.", false))
                }

                div.card.danger {
                    div.card-header { "Danger zone" }
                    div.danger-row {
                        div {
                            strong { "Archive this repository" }
                            p.muted { "Make it read-only." }
                        }
                        button.btn-danger-outline type="button" disabled title="Not available yet" { "Archive" }
                    }
                    div.danger-row {
                        div {
                            strong { "Delete this repository" }
                            p.muted { "This cannot be undone." }
                        }
                        button.btn-danger type="button" disabled title="Not available yet" { "Delete" }
                    }
                }
            }
        },
    )
}

/// Load the authorized signer set off the async runtime, since `signers::load`
/// shells out to git and reads the object database synchronously.
async fn load_signers(repo: &Path) -> Result<Vec<git_ents::signers::Signer>, String> {
    let repo = repo.to_owned();
    tokio::task::spawn_blocking(move || git_ents::signers::load(&repo))
        .await
        .map_err(|err| err.to_string())?
        .map_err(|err| err.to_string())
}

/// A Features row with a static toggle reflecting `on`.
fn feature_row(title: &str, desc: &str, on: bool) -> Markup {
    html! {
        div.feature-row {
            div {
                strong { (title) }
                p.muted { (desc) }
            }
            span.toggle.on[on].stub title="Not editable yet" { span.knob {} }
        }
    }
}

/// A Visibility radio row, selected when `on`.
fn visibility_row(title: &str, desc: &str, on: bool) -> Markup {
    html! {
        div.visibility-row.sel[on] {
            span.radio.on[on].stub title="Not editable yet" {}
            div {
                strong { (title) }
                p.muted { (desc) }
            }
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
