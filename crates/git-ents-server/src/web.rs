//! Browser-facing HTML: a small GitHub-style web UI rendered server-side with
//! Maud and styled with Primer CSS (GitHub's own design system). The git
//! smart-HTTP gateway in [`crate::http`] delegates plain browser GETs here.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use maud::{DOCTYPE, Markup, html};
use tokio::process::Command;

use crate::AppState;
use crate::http::{is_bare_repo, valid_segment};

/// Greatest repository nesting depth served: `repo`, `org/repo`, `org/team/repo`.
const MAX_DEPTH: usize = 3;

/// Pinned Primer CSS so the look does not drift with upstream releases.
const PRIMER_CSS: &str = "https://unpkg.com/@primer/css@21.5.0/dist/primer.css";

/// Render the page for `path`: the repository index at the root, or a single
/// repository's overview otherwise. `host` is the request's `Host` header, used
/// to build a copy-pasteable clone URL.
pub(crate) async fn render(state: &AppState, path: &str, host: Option<&str>) -> Response {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if segments.is_empty() {
        return index(state).into_response();
    }
    if segments.len() > MAX_DEPTH || !segments.iter().all(|s| valid_segment(s)) {
        return not_found().into_response();
    }

    let relative: PathBuf = segments.iter().collect();
    let repo = state.data_dir.join(&relative);
    if !is_bare_repo(&repo) {
        return not_found().into_response();
    }

    let rel_str = segments.join("/");
    repo_page(&repo, &rel_str, host).await.into_response()
}

/// The repository listing shown at `/`.
fn index(state: &AppState) -> Markup {
    let repos = discover_repos(&state.data_dir);
    page(
        "Repositories",
        html! {
            div."d-flex"."flex-items-center"."mb-3" {
                h2.f3.text-normal."flex-auto" { "Repositories" }
            }
            @if repos.is_empty() {
                div.blankslate."color-bg-subtle"."rounded-2" {
                    h3.blankslate-heading { "No repositories yet" }
                    p { "Push to this server to create one:" }
                    p { code { "git push <url>/my-repo.git HEAD" } }
                }
            } @else {
                div.Box {
                    @for repo in &repos {
                        div.Box-row."d-flex"."flex-items-center" {
                            span.mr-2 { "📦" }
                            a.text-bold.flex-auto href={ "/" (repo) } { (repo) }
                            span.Label."Label--secondary" { "git" }
                        }
                    }
                }
            }
        },
    )
}

/// A single repository's overview: default branch, recent commits, root tree.
async fn repo_page(repo: &Path, rel: &str, host: Option<&str>) -> Markup {
    let branch = git_output(repo, &["symbolic-ref", "--short", "HEAD"])
        .await
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    let commits = recent_commits(repo).await;
    let tree = root_tree(repo, branch.is_some()).await;
    let clone_url = clone_url(host, rel);
    let name = rel.rsplit('/').next().unwrap_or(rel);

    page(
        name,
        html! {
            div."d-flex"."flex-items-center"."mb-3" {
                h2.f3.text-normal."flex-auto" {
                    "📂 " a.text-bold href={ "/" (rel) } { (rel) }
                    @if let Some(branch) = &branch {
                        " " span.Label."Label--accent" { (branch) }
                    }
                }
            }

            div.Box."mb-4" {
                div.Box-header { span.text-bold { "Clone" } }
                div.Box-body { code { "git clone " (clone_url) } }
            }

            @if commits.is_empty() {
                div.blankslate."color-bg-subtle"."rounded-2" {
                    h3.blankslate-heading { "This repository is empty" }
                    p { "Push a commit to get started." }
                }
            } @else {
                @if !tree.is_empty() {
                    div.Box."mb-4" {
                        div.Box-header { span.text-bold { "Files" } }
                        @for entry in &tree {
                            div.Box-row {
                                span.mr-2 { (if entry.is_dir { "📁" } else { "📄" }) }
                                span { (entry.name) }
                            }
                        }
                    }
                }
                div.Box {
                    div.Box-header { span.text-bold { "Recent commits" } }
                    @for commit in &commits {
                        div.Box-row {
                            div.text-bold { (commit.subject) }
                            div."color-fg-muted".f6 {
                                code.mr-2 { (commit.short) }
                                (commit.author) " · " (commit.when)
                            }
                        }
                    }
                }
            }
        },
    )
}

/// A parsed `git log` entry.
struct Commit {
    short: String,
    author: String,
    when: String,
    subject: String,
}

/// The most recent commits reachable from `HEAD`, newest first.
async fn recent_commits(repo: &Path) -> Vec<Commit> {
    let Some(log) = git_output(
        repo,
        &["log", "-n", "20", "--format=%H%x00%an%x00%ar%x00%s"],
    )
    .await
    else {
        return Vec::new();
    };
    log.lines()
        .filter_map(|line| {
            let mut parts = line.split('\u{0}');
            let hash = parts.next().unwrap_or_default();
            let author = parts.next().unwrap_or_default();
            let when = parts.next().unwrap_or_default();
            let subject = parts.next().unwrap_or_default();
            if hash.is_empty() {
                return None;
            }
            Some(Commit {
                short: hash.get(..7).unwrap_or(hash).to_owned(),
                author: author.to_owned(),
                when: when.to_owned(),
                subject: subject.to_owned(),
            })
        })
        .collect()
}

/// A single entry in the repository's root tree.
struct TreeEntry {
    name: String,
    is_dir: bool,
}

/// The entries of the root tree at `HEAD`, directories first then by name.
async fn root_tree(repo: &Path, has_head: bool) -> Vec<TreeEntry> {
    if !has_head {
        return Vec::new();
    }
    let Some(out) = git_output(repo, &["ls-tree", "HEAD"]).await else {
        return Vec::new();
    };
    let mut entries: Vec<TreeEntry> = out
        .lines()
        .filter_map(|line| {
            let (meta, name) = line.split_once('\t')?;
            let kind = meta.split(' ').nth(1).unwrap_or_default();
            Some(TreeEntry {
                name: name.to_owned(),
                is_dir: kind == "tree",
            })
        })
        .collect();
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
    entries
}

/// Run `git -C <repo> <args>` and return its stdout, or `None` on failure.
async fn git_output(repo: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stderr(Stdio::null())
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// All bare repositories under `root`, as relative slash paths, sorted.
fn discover_repos(root: &Path) -> Vec<String> {
    let mut repos = Vec::new();
    collect_repos(root, root, MAX_DEPTH, &mut repos);
    repos.sort();
    repos
}

/// Recurse into `dir` (up to `depth` levels) collecting bare repositories.
fn collect_repos(root: &Path, dir: &Path, depth: usize, out: &mut Vec<String>) {
    if depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if is_bare_repo(&path) {
            if let Ok(rel) = path.strip_prefix(root) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        } else {
            collect_repos(root, &path, depth.saturating_sub(1), out);
        }
    }
}

/// The clone URL for `rel`, using the request host when known.
fn clone_url(host: Option<&str>, rel: &str) -> String {
    match host {
        Some(host) => format!("http://{host}/{rel}"),
        None => format!("/{rel}"),
    }
}

/// A `404` page.
fn not_found() -> (StatusCode, Markup) {
    (
        StatusCode::NOT_FOUND,
        page(
            "Not found",
            html! {
                div.blankslate {
                    h3.blankslate-heading { "404" }
                    p { "No such repository." }
                    a.btn href="/" { "Back to repositories" }
                }
            },
        ),
    )
}

/// Wrap page `body` in the shared HTML shell, navigation, and Primer styling.
fn page(title: &str, body: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " · Git Ents" }
                link rel="stylesheet" href=(PRIMER_CSS);
            }
            body."color-bg-default"."color-fg-default" {
                header.Header."color-bg-inset" {
                    div.Header-item {
                        a.Header-link.f4.text-bold href="/" { "🌳 Git Ents" }
                    }
                }
                div."container-lg"."p-responsive"."my-5" {
                    (body)
                }
            }
        }
    }
}
