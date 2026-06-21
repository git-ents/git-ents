//! Browser-facing HTML: a small, hand-styled web UI rendered server-side with
//! Maud. The look mirrors <https://jdc.pub>: DM Sans / Lora / IBM Plex Mono on a
//! warm-gold palette that follows the system light/dark preference. The git
//! smart-HTTP gateway in [`crate::http`] delegates plain browser GETs here.
//!
//! The module is split by concern: [`assets`] bundles the CSS/JS, [`icons`]
//! holds the inline SVGs, [`git`] is the data layer over `git`, and [`pages`]
//! renders each tab. This file owns routing and the shared page shell.

mod assets;
mod git;
mod icons;
mod pages;

use std::path::{Path, PathBuf};

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use maud::{DOCTYPE, Markup, PreEscaped, html};

use crate::AppState;
use crate::http::{MAX_REPO_DEPTH, is_bare_repo, valid_segment};

use self::assets::{COPY_SCRIPT, FONTS, STYLE};
use self::git::{discover_repos, git_output, git_output_bytes};
use self::icons::{icon_branch, icon_chevron, icon_folder, icon_logo, icon_repo, icon_search};

/// Render the page for `path`: the repository index at the root, a repository
/// overview, or one of its browse views (`tree`, `blob`, `commit`). `host` is
/// the request's `Host` header, used to build a copy-pasteable clone URL.
pub(crate) async fn render(state: &AppState, path: &str, host: Option<&str>) -> Response {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if segments.is_empty() {
        return index(state).into_response();
    }

    // The repository is the shortest valid prefix (up to `MAX_REPO_DEPTH`
    // segments) that names a bare repo on disk; anything after it selects a
    // browse view. Resolving the boundary this way keeps a repo named
    // `tree`/`blob`/`commit` distinct from the route markers of the same name.
    let depth_limit = segments.len().min(MAX_REPO_DEPTH);
    for depth in 1..=depth_limit {
        let Some(repo_segs) = segments.get(..depth) else {
            break;
        };
        if !repo_segs.iter().all(|s| valid_segment(s)) {
            break;
        }
        let relative: PathBuf = repo_segs.iter().collect();
        let repo = state.data_dir.join(&relative);
        if !is_bare_repo(&repo) {
            continue;
        }
        let rel = repo_segs.join("/");
        let rest = segments.get(depth..).unwrap_or_default();
        return route(&repo, &rel, rest, host).await;
    }

    not_found().into_response()
}

/// Dispatch the part of the path that follows the repository to a browse view.
/// Each top-level tab is its own route, since the product is server-rendered
/// with no client JavaScript.
async fn route(repo: &Path, rel: &str, rest: &[&str], host: Option<&str>) -> Response {
    let meta = gather_meta(repo, rel).await;
    match rest.split_first() {
        None => pages::repo_page(repo, &meta, host).await.into_response(),
        Some((&"files", sub)) => pages::files_page(repo, &meta, sub).await,
        Some((&"tree", sub)) => pages::tree_page(repo, &meta, sub).await,
        Some((&"blob", sub)) => pages::blob_page(repo, &meta, sub).await,
        Some((&"commit", &[sha])) => pages::commit_page(repo, &meta, sha).await,
        Some((&"releases", &[])) => pages::releases_page(repo, &meta).await.into_response(),
        Some((&"checks", &[])) => pages::checks_page(&meta).into_response(),
        Some((&"issues", &[])) => pages::issues_page(&meta).into_response(),
        Some((&"settings", &[])) => pages::settings_page(&meta).into_response(),
        _ => not_found().into_response(),
    }
}

/// The top-level tabs of a repository page.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Overview,
    Files,
    Releases,
    Checks,
    Issues,
    Settings,
}

/// Metadata shown in the repository header band and tab bar, gathered once per
/// request and shared by every view.
struct RepoMeta {
    rel: String,
    branch: Option<String>,
    description: Option<String>,
    topics: Vec<String>,
    releases: usize,
    issues: usize,
}

impl RepoMeta {
    /// The repository's short name: the final segment of its path.
    fn name(&self) -> &str {
        self.rel.rsplit('/').next().unwrap_or(&self.rel)
    }
}

/// Collect the header/tab metadata for the repository at `rel`.
async fn gather_meta(repo: &Path, rel: &str) -> RepoMeta {
    let branch = git_output(repo, &["symbolic-ref", "--short", "HEAD"])
        .await
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    let description = std::fs::read_to_string(repo.join("description"))
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty() && !s.starts_with("Unnamed repository"));
    let topics = git_output_bytes(repo, &["cat-file", "-p", "HEAD:.gitents/topics"])
        .await
        .map(|b| {
            String::from_utf8_lossy(&b)
                .split([',', '\n', ' ', '\t'])
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let releases = git_output(repo, &["tag", "--list"])
        .await
        .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count())
        .unwrap_or(0);
    RepoMeta {
        rel: rel.to_owned(),
        branch,
        description,
        topics,
        releases,
        issues: 0,
    }
}

/// Wrap a repository view in the shared header band and tab bar, then the page
/// shell. `active` highlights the current tab.
fn repo_shell(meta: &RepoMeta, active: Tab, title: &str, body: Markup) -> Markup {
    page(
        title,
        html! { (repo_header(meta)) (tab_bar(meta, active)) (body) },
    )
}

/// The repository header band: path line, branch and visibility pills,
/// description, and topic chips.
fn repo_header(meta: &RepoMeta) -> Markup {
    let segments: Vec<&str> = meta.rel.split('/').collect();
    let last = segments.len().saturating_sub(1);
    html! {
        div.repo-header {
            div.repo-headline {
                div.repo-path {
                    (icon_folder())
                    @for (i, seg) in segments.iter().enumerate() {
                        @if i > 0 { span.sep { "/" } }
                        @if i == last {
                            span.here { (seg) }
                        } @else {
                            @let href = format!("/{}", segments.get(..=i).unwrap_or_default().join("/"));
                            a href=(href) { (seg) }
                        }
                    }
                    @if let Some(branch) = &meta.branch {
                        span.branch { (icon_branch()) (branch) }
                    }
                    span.pill-public { "Public" }
                }
                @if let Some(desc) = &meta.description {
                    p.repo-desc { (desc) }
                }
                @if !meta.topics.is_empty() {
                    div.topics {
                        @for topic in &meta.topics {
                            span.topic { (topic) }
                        }
                    }
                }
            }
        }
    }
}

/// The tab bar with the active tab underlined. Tabs that have no backing data
/// yet still render so the navigation matches the design.
fn tab_bar(meta: &RepoMeta, active: Tab) -> Markup {
    let rel = &meta.rel;
    html! {
        nav.tabs {
            a.tab.active[active == Tab::Overview] href={ "/" (rel) } { "Overview" }
            a.tab.active[active == Tab::Files] href={ "/" (rel) "/files" } { "Files" }
            a.tab.active[active == Tab::Releases] href={ "/" (rel) "/releases" } {
                "Releases"
                @if meta.releases > 0 { span.tab-count { (meta.releases) } }
            }
            a.tab.active[active == Tab::Checks] href={ "/" (rel) "/checks" } { "Checks" }
            a.tab.active[active == Tab::Issues] href={ "/" (rel) "/issues" } {
                "Issues"
                @if meta.issues > 0 { span.tab-count { (meta.issues) } }
            }
            a.tab.active[active == Tab::Settings] href={ "/" (rel) "/settings" } { "Settings" }
        }
    }
}

/// The repository listing shown at `/`.
fn index(state: &AppState) -> Markup {
    let repos = discover_repos(&state.data_dir);
    page(
        "Repositories",
        html! {
            div.page-header {
                h1.page-title { (icon_repo()) "Repositories" }
                @if !repos.is_empty() {
                    span.count { (repos.len()) " repos" }
                }
            }
            @if repos.is_empty() {
                div.blankslate {
                    h2 { "No repositories yet" }
                    p { "Push to this server to create one:" }
                    p { code { "git push <url>/my-repo.git HEAD" } }
                }
            } @else {
                ul.repo-list {
                    @for repo in &repos {
                        li {
                            a.repo-row href={ "/" (repo) } {
                                span.repo-icon { (icon_repo()) }
                                span.repo-name { (repo) }
                                span.repo-badge { "git" }
                                span.repo-arrow { (icon_chevron()) }
                            }
                        }
                    }
                }
            }
        },
    )
}

/// A `404` page.
fn not_found() -> (StatusCode, Markup) {
    (
        StatusCode::NOT_FOUND,
        page(
            "Not found",
            html! {
                div.blankslate {
                    h2 { "404" }
                    p { "No such repository." }
                    a.btn href="/" { "Back to repositories" }
                }
            },
        ),
    )
}

/// Wrap page `body` in the shared HTML shell, navigation, and styling.
fn page(title: &str, body: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " · Git Ents" }
                link rel="preconnect" href="https://fonts.googleapis.com";
                link rel="preconnect" href="https://fonts.gstatic.com" crossorigin;
                link rel="stylesheet" href=(FONTS);
                style { (PreEscaped(STYLE)) }
            }
            body {
                nav.site-nav {
                    div.nav-inner {
                        a.nav-logo href="/" { (icon_logo()) "git-ents" }
                        div.nav-search {
                            (icon_search())
                            input type="search" placeholder="Jump to file or symbol" aria-label="Search" disabled title="Not available yet";
                        }
                    }
                }
                main.content { (body) }
                footer.site-footer {
                    div.footer-inner {
                        "git-ents · served as paper-grain HTML · no JavaScript required"
                    }
                }
                script { (PreEscaped(COPY_SCRIPT)) }
            }
        }
    }
}
