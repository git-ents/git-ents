//! Browser-facing HTML: a small, hand-styled web UI rendered server-side with
//! Maud. The look mirrors <https://jdc.pub>: DM Sans / Lora / IBM Plex Mono on a
//! warm-gold palette that follows the system light/dark preference. The git
//! smart-HTTP gateway in [`crate::http`] delegates plain browser GETs here.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use arborium::{Config, Highlighter, HtmlFormat};
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
  --s-comment: #9c8f74;
  --s-keyword: #9d0006;
  --s-func: #427b58;
  --s-type: #b57614;
  --s-string: #79740e;
  --s-const: #8f3f71;
  --s-op: #7c6f57;
  --s-prop: #076678;
  --diff-add: #4e9a0622;
  --diff-del: #cc241d22;
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
    --s-comment: #928374;
    --s-keyword: #fb4934;
    --s-func: #8ec07c;
    --s-type: #fabd2f;
    --s-string: #b8bb26;
    --s-const: #d3869b;
    --s-op: #a89984;
    --s-prop: #83a598;
    --diff-add: #b8bb2620;
    --diff-del: #fb493420;
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

.card-row a { color: inherit; text-decoration: none; flex: 1; min-width: 0; word-break: break-all; }
.card-row a:hover { color: var(--color-accent); }
.commit-subject a { color: inherit; text-decoration: none; }
.commit-subject a:hover { color: var(--color-accent); }

.crumbs { font-family: var(--font-mono); font-size: .92rem; margin-bottom: 1.25rem; display: flex; flex-wrap: wrap; align-items: center; gap: .3rem; word-break: break-all; }
.crumbs a { text-decoration: none; }
.crumbs .sep { color: var(--color-text-muted); opacity: .55; }
.crumbs .here { color: var(--color-text-muted); }

.blob { display: grid; grid-template-columns: auto minmax(0, 1fr); background: var(--color-surface); border: 1px solid var(--color-border); border-radius: var(--radius-sm); box-shadow: var(--shadow-sm); overflow: hidden; margin-bottom: 1.5rem; }
.blob pre { font-family: var(--font-mono); font-size: .82rem; line-height: 1.55; margin: 0; padding: 1rem 0; }
.blob-nums { text-align: right; color: var(--color-text-muted); background: var(--color-code-bg); border-right: 1px solid var(--color-border); padding-left: 1rem; padding-right: 1rem; user-select: none; -webkit-user-select: none; }
.blob-code { overflow-x: auto; min-width: 0; }
.blob-code code { display: block; font-family: inherit; padding: 0 1rem; white-space: pre; color: var(--color-text); }
.binary { padding: 2.5rem; text-align: center; font-family: var(--font-mono); font-size: .85rem; color: var(--color-text-muted); }

.code .keyword, .code .macro, .code .tag { color: var(--s-keyword); }
.code .function, .code .constructor { color: var(--s-func); }
.code .type { color: var(--s-type); }
.code .string { color: var(--s-string); }
.code .number, .code .constant, .code .label { color: var(--s-const); }
.code .comment { color: var(--s-comment); font-style: italic; }
.code .operator, .code .punctuation { color: var(--s-op); }
.code .property, .code .attribute { color: var(--s-prop); }
.code .title { color: var(--s-keyword); font-weight: 700; }
.code .strong { font-weight: 700; }
.code .emphasis { font-style: italic; }
.code .link, .code .url, .code .reference { color: var(--s-prop); text-decoration: underline; }
.code .markup { color: var(--s-func); }

.diff { background: var(--color-surface); border: 1px solid var(--color-border); border-radius: var(--radius-sm); box-shadow: var(--shadow-sm); overflow-x: auto; margin-bottom: 1.5rem; font-family: var(--font-mono); font-size: .82rem; line-height: 1.55; padding: .6rem 0; }
.diff .ln { display: block; padding: 0 1rem; white-space: pre; }
.diff .add { background: var(--diff-add); }
.diff .del { background: var(--diff-del); }
.diff .hunk { color: var(--s-prop); background: var(--color-code-bg); }
.diff .meta { color: var(--color-text-muted); }
.diff .file { color: var(--color-text); font-weight: 600; background: var(--color-code-bg); padding-top: .3rem; padding-bottom: .3rem; }

.commit-msg { font-family: var(--font-mono); font-size: .9rem; white-space: pre-wrap; word-break: break-word; }

.adoc-body { padding: 40px 48px 52px; max-width: 44rem; overflow-wrap: break-word; }
.adoc-body > :first-child { margin-top: 0; }
.adoc-body h1, .adoc-body h2, .adoc-body h3, .adoc-body h4 { font-family: var(--font-serif); font-weight: 700; letter-spacing: -.01em; line-height: 1.25; margin: 1.8rem 0 .9rem; }
.adoc-body h1 { font-size: 2.4rem; letter-spacing: -.02em; }
.adoc-body h2 { font-size: 1.4rem; font-weight: 600; position: relative; padding-bottom: .55rem; }
.adoc-body h2::after { content: ""; position: absolute; left: 0; bottom: 0; width: 3rem; height: 2px; background: var(--color-accent); border-radius: 1px; }
.adoc-body h3 { font-size: 1.15rem; font-weight: 600; }
.adoc-body p, .adoc-body ul, .adoc-body ol { margin: 0 0 1rem; }
.adoc-body ul, .adoc-body ol { padding-left: 1.4rem; }
.adoc-body li { margin: .25rem 0; }
.adoc-body a { font-weight: 500; }
.adoc-body code, .adoc-body .literal { font-family: var(--font-mono); font-size: .86em; background: var(--color-code-bg); padding: .1rem .35rem; border-radius: 5px; }
.adoc-body pre { font-family: var(--font-mono); font-size: .82rem; line-height: 1.55; background: var(--color-code-bg); border: 1px solid var(--color-border); border-radius: var(--radius-sm); padding: 1rem 1.2rem; overflow-x: auto; margin: 0 0 1rem; }
.adoc-body pre code { background: none; padding: 0; font-size: inherit; }
.adoc-body blockquote { border-left: 3px solid var(--color-accent); padding: .2rem 0 .2rem 1.1rem; margin: 0 0 1rem; color: var(--color-text-muted); }
.adoc-body table { border-collapse: collapse; margin: 0 0 1rem; font-size: .92rem; }
.adoc-body th, .adoc-body td { border: 1px solid var(--color-border); padding: .4rem .7rem; text-align: left; }
.adoc-body th { background: var(--color-code-bg); font-weight: 600; }
.adoc-body .title { font-weight: 600; color: var(--color-text-muted); font-size: .9rem; margin-bottom: .3rem; }
.adoc-body img { max-width: 100%; height: auto; }
.adoc-body hr { border: none; border-top: 1px solid var(--color-border); margin: 1.8rem 0; }

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

/// Render the page for `path`: the repository index at the root, a repository
/// overview, or one of its browse views (`tree`, `blob`, `commit`). `host` is
/// the request's `Host` header, used to build a copy-pasteable clone URL.
pub(crate) async fn render(state: &AppState, path: &str, host: Option<&str>) -> Response {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if segments.is_empty() {
        return index(state).into_response();
    }

    // The repository is the shortest valid prefix (up to `MAX_DEPTH` segments)
    // that names a bare repo on disk; anything after it selects a browse view.
    // Resolving the boundary this way keeps a repo named `tree`/`blob`/`commit`
    // distinct from the route markers of the same name.
    let depth_limit = segments.len().min(MAX_DEPTH);
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
async fn route(repo: &Path, rel: &str, rest: &[&str], host: Option<&str>) -> Response {
    match rest.split_first() {
        None => repo_page(repo, rel, host).await.into_response(),
        Some((&"tree", sub)) => tree_page(repo, rel, sub).await,
        Some((&"blob", sub)) => blob_page(repo, rel, sub).await,
        Some((&"commit", &[sha])) => commit_page(repo, rel, sha).await,
        _ => not_found().into_response(),
    }
}

/// Join the path segments of a browse view, rejecting empty or traversing
/// components. The result is used only as a git tree path (`HEAD:<path>`), never
/// touched on disk, but refusing `..` keeps the rendered links well-formed.
fn browse_path(sub: &[&str]) -> Option<String> {
    if sub.iter().any(|s| s.is_empty() || *s == "." || *s == "..") {
        return None;
    }
    Some(sub.join("/"))
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
    let readme = readme(repo, &tree).await;
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
                @if let Some((file, html)) = &readme {
                    div.card {
                        div.card-header { (file) }
                        article.adoc-body { (PreEscaped(html)) }
                    }
                }
                @if !tree.is_empty() {
                    div.card {
                        div.card-header { "Files" }
                        @for entry in &tree {
                            div.card-row.is-dir[entry.is_dir] {
                                @if entry.is_dir { (icon_folder()) } @else { (icon_file()) }
                                a href=(entry_href(rel, "", entry)) { (entry.name) }
                            }
                        }
                    }
                }
                div.card {
                    div.card-header { "Recent commits" }
                    @for commit in &commits {
                        div.commit {
                            div.commit-subject {
                                a href={ "/" (rel) "/commit/" (commit.hash) } { (commit.subject) }
                            }
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
    hash: String,
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
                hash: hash.to_owned(),
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
    list_tree(repo, "HEAD").await
}

/// The rendered README for the overview: the first AsciiDoc file in the root
/// tree whose stem is `README`, converted to HTML, paired with its filename.
/// `None` when there is no such file or it fails to render.
async fn readme(repo: &Path, tree: &[TreeEntry]) -> Option<(String, String)> {
    let entry = tree.iter().find(|e| {
        !e.is_dir
            && crate::asciidoc::is_asciidoc(&e.name)
            && e.name
                .rsplit_once('.')
                .is_some_and(|(stem, _)| stem.eq_ignore_ascii_case("readme"))
    })?;
    let spec = format!("HEAD:{}", entry.name);
    let bytes = git_output_bytes(repo, &["cat-file", "-p", &spec]).await?;
    let html = crate::asciidoc::to_html(&String::from_utf8_lossy(&bytes))?;
    Some((entry.name.clone(), html))
}

/// The entries of the tree named by `spec` (a git tree-ish such as `HEAD` or
/// `HEAD:src`), directories first then by name. Empty if `spec` is not a tree.
async fn list_tree(repo: &Path, spec: &str) -> Vec<TreeEntry> {
    let Some(out) = git_output(repo, &["ls-tree", spec]).await else {
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

/// The link to a tree entry: a `tree` view for directories, a `blob` view for
/// files. `dir` is the tree's path within the repo (empty at the root).
fn entry_href(rel: &str, dir: &str, entry: &TreeEntry) -> String {
    let view = if entry.is_dir { "tree" } else { "blob" };
    let name = &entry.name;
    if dir.is_empty() {
        format!("/{rel}/{view}/{name}")
    } else {
        format!("/{rel}/{view}/{dir}/{name}")
    }
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

/// Run `git -C <repo> <args>` and return its raw stdout bytes, or `None` on
/// failure. Used for blob contents, which may not be valid UTF-8.
async fn git_output_bytes(repo: &Path, args: &[&str]) -> Option<Vec<u8>> {
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
    Some(out.stdout)
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

/// A directory listing at `sub` within the repository.
async fn tree_page(repo: &Path, rel: &str, sub: &[&str]) -> Response {
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
    let name = rel.rsplit('/').next().unwrap_or(rel);
    page(
        name,
        html! {
            (crumbs(rel, &dir, false))
            div.card {
                div.card-header { "Files" }
                @if dir.is_empty() && entries.is_empty() {
                    div.card-row { "Empty repository." }
                }
                @for entry in &entries {
                    div.card-row.is-dir[entry.is_dir] {
                        @if entry.is_dir { (icon_folder()) } @else { (icon_file()) }
                        a href=(entry_href(rel, &dir, entry)) { (entry.name) }
                    }
                }
            }
        },
    )
    .into_response()
}

/// A single file's contents at `sub`, syntax-highlighted when the language is
/// recognized and the file is text.
async fn blob_page(repo: &Path, rel: &str, sub: &[&str]) -> Response {
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
    let Some(bytes) = git_output_bytes(repo, &["cat-file", "-p", &spec]).await else {
        return not_found().into_response();
    };
    let name = path.rsplit('/').next().unwrap_or(&path);
    let body = if is_binary(&bytes) {
        html! { div.blob { div.binary { "Binary file (" (bytes.len()) " bytes) not shown." } } }
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
    page(
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
async fn commit_page(repo: &Path, rel: &str, sha: &str) -> Response {
    if sha.is_empty() || sha.len() > 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return not_found().into_response();
    }
    let Some(meta) = git_output(
        repo,
        &["show", "-s", "--format=%H%x00%an%x00%ar%x00%s%x00%b", sha],
    )
    .await
    else {
        return not_found().into_response();
    };
    let mut parts = meta.split('\u{0}');
    let hash = parts.next().unwrap_or_default().trim().to_owned();
    let author = parts.next().unwrap_or_default().to_owned();
    let when = parts.next().unwrap_or_default().to_owned();
    let subject = parts.next().unwrap_or_default().to_owned();
    let body = parts.next().unwrap_or_default().trim_end().to_owned();
    let short = hash.get(..7).unwrap_or(&hash).to_owned();
    let patch = git_output(repo, &["show", "--no-color", "--format=", "--patch", sha])
        .await
        .unwrap_or_default();

    page(
        &subject,
        html! {
            div.page-header {
                h1.page-title { a href={ "/" (rel) } { (rel) } }
                span.branch { (short) }
            }
            div.card {
                div.card-header { "Commit" }
                div.commit {
                    div.commit-subject { (subject) }
                    @if !body.is_empty() {
                        div.commit-msg { (body) }
                    }
                    div.commit-meta { (author) " · " (when) }
                }
            }
            (diff_view(&patch))
        },
    )
    .into_response()
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
