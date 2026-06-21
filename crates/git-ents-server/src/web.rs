//! Browser-facing HTML: a small, hand-styled web UI rendered server-side with
//! Maud. The look mirrors <https://jdc.pub>: DM Sans / Lora / IBM Plex Mono on a
//! warm-gold palette that follows the system light/dark preference. The git
//! smart-HTTP gateway in [`crate::http`] delegates plain browser GETs here.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use maud::{DOCTYPE, Markup, PreEscaped, html};
use tokio::process::Command;

use crate::AppState;
use crate::http::{is_bare_repo, valid_segment};

/// Greatest repository nesting depth served: `repo`, `org/repo`, `org/team/repo`.
const MAX_DEPTH: usize = 3;

/// Web fonts, matching the typography of <https://jdc.pub>.
const FONTS: &str = "https://fonts.googleapis.com/css2?family=DM+Sans:wght@400;500;600;700&family=IBM+Plex+Mono:wght@400;500;600&family=Lora:wght@500;600;700&display=swap";

/// Hand-written stylesheet (no external CSS framework) so the look stays stable
/// and self-contained. Colors, type, and radii track <https://jdc.pub>, with a
/// `prefers-color-scheme` block for automatic dark mode.
const STYLE: &str = r#"
:root {
  --font-sans: "DM Sans", system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
  --font-serif: "Lora", Georgia, "Times New Roman", serif;
  --font-mono: "IBM Plex Mono", ui-monospace, "Cascadia Code", "Source Code Pro", Menlo, monospace;
  --max-width: 52rem;
  --color-bg: #faf8f4;
  --color-surface: #fff;
  --color-text: #2a2518;
  --color-text-muted: #8a7e6a;
  --color-link: #b07d10;
  --color-link-hover: #96690a;
  --color-border: #ede9de;
  --color-code-bg: #f5f3eb;
  --color-accent: #b07d10;
  --color-accent-subtle: #b07d100f;
  --shadow-sm: 0 1px 3px #0000000d;
  --shadow-md: 0 4px 16px #0000000f;
  --radius-sm: 10px;
  --radius-pill: 100px;
  --glow: #b07d1012;
}
@media (prefers-color-scheme: dark) {
  :root {
    --color-bg: #171510;
    --color-surface: #211f17;
    --color-text: #ede8d8;
    --color-text-muted: #a89e88;
    --color-link: #d4a030;
    --color-link-hover: #e4b850;
    --color-border: #383324;
    --color-code-bg: #211f17;
    --color-accent: #d4a030;
    --color-accent-subtle: #d4a03012;
    --shadow-sm: 0 1px 3px #00000040;
    --shadow-md: 0 4px 16px #0000004d;
    --glow: #d4a03014;
  }
}
*, *::before, *::after { box-sizing: border-box; margin: 0; padding: 0; }
html { font-size: 17px; -webkit-font-smoothing: antialiased; -moz-osx-font-smoothing: grayscale; }
body {
  font-family: var(--font-sans);
  background: var(--color-bg);
  color: var(--color-text);
  line-height: 1.7;
  min-height: 100vh;
  display: flex;
  flex-direction: column;
  background-image: radial-gradient(80% 40% at 50% -10%, var(--glow) 0%, #0000 70%);
}
a { color: var(--color-link); text-decoration: underline; text-decoration-color: color-mix(in srgb, var(--color-link) 25%, transparent); text-underline-offset: 2px; transition: color .15s, text-decoration-color .15s; }
a:hover { color: var(--color-link-hover); text-decoration-color: currentColor; }
.icon { flex-shrink: 0; fill: currentColor; vertical-align: -0.125em; }

.site-nav { position: sticky; top: 0; z-index: 100; background: color-mix(in srgb, var(--color-bg) 82%, transparent); backdrop-filter: blur(10px); border-bottom: 1px solid var(--color-border); }
.nav-inner { max-width: var(--max-width); margin: 0 auto; padding: 1rem 1.5rem; display: flex; align-items: center; gap: 2rem; }
.nav-logo { font-family: var(--font-mono); font-weight: 700; font-size: 1.05rem; color: var(--color-text); letter-spacing: -.01em; text-decoration: none; margin-right: auto; transition: color .15s; }
.nav-logo:hover { color: var(--color-accent); }

.content { max-width: var(--max-width); width: 100%; margin: 0 auto; padding: 2.25rem 1.5rem 3rem; flex: 1; }

.page-header { margin-bottom: 1.75rem; padding-bottom: 1.25rem; border-bottom: 1px solid var(--color-border); position: relative; display: flex; align-items: center; gap: .75rem; flex-wrap: wrap; }
.page-header::after { content: ""; position: absolute; bottom: -1px; left: 0; width: 3rem; height: 2px; background: var(--color-accent); border-radius: 1px; }
.page-title { font-family: var(--font-serif); font-size: 1.5rem; font-weight: 700; letter-spacing: -.01em; line-height: 1.3; display: inline-flex; align-items: center; gap: .55rem; }
.page-title .icon { color: var(--color-accent); width: 20px; height: 20px; }
.page-title a { color: inherit; text-decoration: none; }
.page-title a:hover { color: var(--color-accent); }
.count { margin-left: auto; font-family: var(--font-mono); font-size: .8rem; color: var(--color-text-muted); }

.branch { font-family: var(--font-mono); font-size: .72rem; font-weight: 600; color: var(--color-accent); background: var(--color-accent-subtle); border: 1px solid color-mix(in srgb, var(--color-accent) 30%, transparent); border-radius: var(--radius-pill); padding: .1rem .6rem; }

.repo-list { list-style: none; }
.repo-list li + li { border-top: 1px solid var(--color-border); }
.repo-row { display: flex; align-items: center; gap: .85rem; padding: .9rem .75rem; border-radius: var(--radius-sm); text-decoration: none; color: inherit; transition: background .18s, transform .18s; }
.repo-row:hover { text-decoration: none; transform: translateX(3px); }
.repo-row:hover .repo-name { color: var(--color-accent); }
.repo-row:hover .repo-arrow { opacity: 1; transform: translateX(0); }
.repo-row .repo-icon { color: var(--color-accent); display: inline-flex; }
.repo-name { font-family: var(--font-mono); font-size: 1rem; font-weight: 600; flex: 1; min-width: 0; transition: color .18s; word-break: break-all; }
.repo-badge { font-family: var(--font-mono); font-size: .68rem; font-weight: 600; text-transform: uppercase; letter-spacing: .05em; color: var(--color-text-muted); border: 1px solid var(--color-border); border-radius: var(--radius-pill); padding: .08rem .55rem; }
.repo-arrow { color: var(--color-accent); display: inline-flex; opacity: 0; transform: translateX(-6px); transition: opacity .22s, transform .22s; }

.card { background: var(--color-surface); border: 1px solid var(--color-border); border-radius: var(--radius-sm); box-shadow: var(--shadow-sm); margin-bottom: 1.5rem; overflow: hidden; }
.card-header { font-family: var(--font-mono); font-size: .72rem; font-weight: 600; text-transform: uppercase; letter-spacing: .06em; color: var(--color-text-muted); background: var(--color-code-bg); padding: .55rem 1.1rem; border-bottom: 1px solid var(--color-border); }
.card-row { display: flex; align-items: center; gap: .65rem; padding: .7rem 1.1rem; font-family: var(--font-mono); font-size: .9rem; }
.card-row + .card-row { border-top: 1px solid var(--color-border); }
.card-row .icon { color: var(--color-text-muted); }
.card-row.is-dir .icon { color: var(--color-accent); }

.commit { padding: .85rem 1.1rem; }
.commit + .commit { border-top: 1px solid var(--color-border); }
.commit-subject { font-weight: 600; line-height: 1.45; }
.commit-meta { font-size: .8rem; color: var(--color-text-muted); margin-top: .15rem; }
.commit-meta .sha { font-family: var(--font-mono); background: var(--color-code-bg); padding: .08rem .4rem; border-radius: 5px; font-size: .76rem; margin-right: .5rem; }

.clone { display: flex; align-items: stretch; }
.clone code { flex: 1; font-family: var(--font-mono); font-size: .82rem; background: var(--color-code-bg); padding: .7rem 1rem; overflow-x: auto; white-space: pre; color: var(--color-text); }
.copy-btn { font-family: var(--font-mono); font-size: .74rem; font-weight: 600; border: none; border-left: 1px solid var(--color-border); background: var(--color-surface); color: var(--color-text-muted); padding: 0 1rem; cursor: pointer; transition: color .15s, background .15s; }
.copy-btn:hover { color: var(--color-accent); background: var(--color-accent-subtle); }

.blankslate { text-align: center; padding: 3rem 1.5rem; background: var(--color-surface); border: 1px solid var(--color-border); border-radius: var(--radius-sm); box-shadow: var(--shadow-sm); }
.blankslate h2 { font-family: var(--font-serif); font-size: 1.3rem; font-weight: 700; margin-bottom: .5rem; }
.blankslate p { color: var(--color-text-muted); }
.blankslate code { font-family: var(--font-mono); background: var(--color-code-bg); padding: .15rem .45rem; border-radius: 5px; font-size: .85rem; }
.btn { display: inline-flex; align-items: center; gap: .4rem; margin-top: 1.25rem; font-size: .88rem; font-weight: 600; color: var(--color-accent); text-decoration: none; padding: .45rem 1rem; border-radius: var(--radius-sm); border: 1px solid var(--color-border); background: var(--color-surface); box-shadow: var(--shadow-sm); transition: border-color .15s, box-shadow .15s; }
.btn:hover { text-decoration: none; border-color: var(--color-accent); box-shadow: var(--shadow-md); }

.site-footer { border-top: 1px solid var(--color-border); color: var(--color-text-muted); font-size: .8rem; margin-top: auto; }
.footer-inner { max-width: var(--max-width); margin: 0 auto; padding: 2rem 1.5rem; text-align: center; }
.footer-inner a { color: var(--color-text-muted); text-decoration: none; }
.footer-inner a:hover { color: var(--color-accent); }

@media (max-width: 640px) {
  html { font-size: 16px; }
  .content { padding: 1.5rem 1.25rem 2.5rem; }
}
"#;

/// Clipboard handler for the clone-URL copy button.
const COPY_SCRIPT: &str = r#"
document.querySelectorAll('[data-copy]').forEach((btn) => {
  btn.addEventListener('click', () => {
    navigator.clipboard.writeText(btn.dataset.copy).then(() => {
      const label = btn.textContent;
      btn.textContent = 'Copied';
      setTimeout(() => { btn.textContent = label; }, 1200);
    });
  });
});
"#;

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
                                span.repo-arrow { (icon_arrow()) }
                            }
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
            div.page-header {
                h1.page-title {
                    (icon_folder())
                    a href={ "/" (rel) } { (rel) }
                }
                @if let Some(branch) = &branch {
                    span.branch { (branch) }
                }
            }

            div.card {
                div.card-header { "Clone" }
                div.clone {
                    code { "git clone " (clone_url) }
                    button.copy-btn data-copy={ "git clone " (clone_url) } { "Copy" }
                }
            }

            @if commits.is_empty() {
                div.blankslate {
                    h2 { "This repository is empty" }
                    p { "Push a commit to get started." }
                }
            } @else {
                @if !tree.is_empty() {
                    div.card {
                        div.card-header { "Files" }
                        @for entry in &tree {
                            div.card-row.is-dir[entry.is_dir] {
                                @if entry.is_dir { (icon_folder()) } @else { (icon_file()) }
                                span { (entry.name) }
                            }
                        }
                    }
                }
                div.card {
                    div.card-header { "Recent commits" }
                    @for commit in &commits {
                        div.commit {
                            div.commit-subject { (commit.subject) }
                            div.commit-meta {
                                span.sha { (commit.short) }
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
                        a.nav-logo href="/" { "🌳 Git Ents" }
                    }
                }
                main.content { (body) }
                footer.site-footer {
                    div.footer-inner {
                        "Served by " a href="/" { "Git Ents" } "."
                    }
                }
                script { (PreEscaped(COPY_SCRIPT)) }
            }
        }
    }
}

/// Inline icons (16×16 Octicons paths), kept local so the UI has no asset deps.
fn svg(path: &str) -> Markup {
    html! {
        svg.icon viewBox="0 0 16 16" width="16" height="16" aria-hidden="true" {
            (PreEscaped(format!("<path d=\"{path}\"/>")))
        }
    }
}

fn icon_repo() -> Markup {
    svg(
        "M2 2.5A2.5 2.5 0 0 1 4.5 0h8.75a.75.75 0 0 1 .75.75v12.5a.75.75 0 0 1-.75.75h-2.5a.75.75 0 0 1 0-1.5h1.75v-2h-8a1 1 0 0 0-.714 1.7.75.75 0 1 1-1.072 1.05A2.495 2.495 0 0 1 2 11.5Zm10.5-1h-8a1 1 0 0 0-1 1v6.708A2.486 2.486 0 0 1 4.5 9h8ZM5 12.25a.25.25 0 0 1 .25-.25h3.5a.25.25 0 0 1 .25.25v3.25a.25.25 0 0 1-.4.2l-1.45-1.087a.249.249 0 0 0-.3 0L5.4 15.7a.25.25 0 0 1-.4-.2Z",
    )
}

fn icon_folder() -> Markup {
    svg(
        "M1.75 1A1.75 1.75 0 0 0 0 2.75v10.5C0 14.216.784 15 1.75 15h12.5A1.75 1.75 0 0 0 16 13.25v-8.5A1.75 1.75 0 0 0 14.25 3H7.5a.25.25 0 0 1-.2-.1l-.9-1.2C6.07 1.26 5.55 1 5 1H1.75Z",
    )
}

fn icon_file() -> Markup {
    svg(
        "M2 1.75C2 .784 2.784 0 3.75 0h6.586c.464 0 .909.184 1.237.513l2.914 2.914c.329.328.513.773.513 1.237v9.586A1.75 1.75 0 0 1 13.25 16h-9.5A1.75 1.75 0 0 1 2 14.25Zm1.75-.25a.25.25 0 0 0-.25.25v12.5c0 .138.112.25.25.25h9.5a.25.25 0 0 0 .25-.25V6h-2.75A1.75 1.75 0 0 1 9 4.25V1.5Zm6.75.062V4.25c0 .138.112.25.25.25h2.688l-.011-.013-2.914-2.914-.013-.011Z",
    )
}

fn icon_arrow() -> Markup {
    svg(
        "M6.22 3.22a.75.75 0 0 1 1.06 0l4.25 4.25a.75.75 0 0 1 0 1.06l-4.25 4.25a.75.75 0 0 1-1.06-1.06L9.94 8 6.22 4.28a.75.75 0 0 1 0-1.06Z",
    )
}
