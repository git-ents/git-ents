//! Browser-facing HTML: a small, hand-styled web UI rendered server-side with
//! Maud. The look mirrors <https://jdc.pub>: DM Sans / Lora / IBM Plex Mono on a
//! warm-gold palette that follows the system light/dark preference. The git
//! smart-HTTP gateway in [`crate::http`] delegates plain browser GETs here.
//!
//! The module is split by concern: [`assets`] bundles the CSS/JS, [`icons`]
//! holds the inline SVGs, [`git`] is the data layer over `git`, [`render`]
//! turns reflected meta-ref values into HTML, and [`pages`] renders each tab.
//! This file owns routing and the shared page shell.

mod assets;
mod git;
mod icons;
mod pages;
mod render;
mod write;

use std::path::{Path, PathBuf};

use axum::body::Bytes;
use axum::http::header::{LOCATION, SET_COOKIE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use maud::{DOCTYPE, Markup, PreEscaped, html};

use crate::AppState;
use crate::http::{MAX_REPO_DEPTH, is_bare_repo, valid_segment};

pub(crate) use self::write::{Sessions, new_sessions};

/// Who is signed in for the current request, resolved per repository: a member's
/// web key authorizes edits only on a repo whose member list contains it.
pub(super) struct Auth {
    /// The session key's display label.
    label: String,
    /// The member username this key maps to in the current repo, when it is a
    /// member there — the gate for showing edit controls.
    username: Option<String>,
}

use self::assets::{COPY_SCRIPT, FONTS, STYLE};
use self::git::{discover_repos, git_output};
use self::icons::{icon_branch, icon_chevron, icon_folder, icon_logo, icon_repo, icon_search};

/// Render the page for `path`: the repository index at the root, a repository
/// overview, or one of its browse views (`tree`, `blob`, `commit`). `host` is
/// the request's `Host` header, used to build a copy-pasteable clone URL.
pub(crate) async fn render(
    state: &AppState,
    path: &str,
    host: Option<&str>,
    cookie: Option<&str>,
) -> Response {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let session = write::snapshot(&state.sessions, cookie);
    if segments.is_empty() {
        return index(state, session.as_ref()).into_response();
    }
    if segments == ["login"] {
        return login_page(session.as_ref(), None).into_response();
    }

    if let Some((repo, rel, rest)) = resolve_repo(&state.data_dir, &segments) {
        return route(&repo, &rel, rest, host, session).await;
    }

    not_found().into_response()
}

/// Resolve the leading path segments to a repository: the shortest valid prefix
/// (up to [`MAX_REPO_DEPTH`] segments) that names a bare repo on disk, with the
/// rest of the path selecting a view. Resolving the boundary this way keeps a
/// repo named `tree`/`blob`/`commit` distinct from the route markers of the same
/// name.
fn resolve_repo<'a>(
    data_dir: &Path,
    segments: &'a [&'a str],
) -> Option<(PathBuf, String, &'a [&'a str])> {
    let depth_limit = segments.len().min(MAX_REPO_DEPTH);
    for depth in 1..=depth_limit {
        let repo_segs = segments.get(..depth)?;
        if !repo_segs.iter().all(|s| valid_segment(s)) {
            break;
        }
        let relative: PathBuf = repo_segs.iter().collect();
        let repo = data_dir.join(&relative);
        if !is_bare_repo(&repo) {
            continue;
        }
        let rel = repo_segs.join("/");
        let rest = segments.get(depth..).unwrap_or_default();
        return Some((repo, rel, rest));
    }
    None
}

/// Handle a browser POST: signing in, signing out, or saving a settings edit.
/// Git wire POSTs never reach here — [`crate::http`] routes those to the backend.
pub(crate) async fn handle_post(
    state: &AppState,
    path: &str,
    headers: &HeaderMap,
    body: Bytes,
) -> Response {
    let cookie = headers
        .get(axum::http::header::COOKIE)
        .and_then(|value| value.to_str().ok());
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    if segments == ["login"] {
        return match write::login(&state.sessions, &body) {
            Ok(token) => redirect("/login", Some(session_cookie(&token))),
            Err(error) => login_page(None, Some(&error)).into_response(),
        };
    }
    if segments == ["logout"] {
        write::logout(&state.sessions, cookie);
        return redirect("/login", Some(cleared_cookie()));
    }

    let Some((repo, rel, rest)) = resolve_repo(&state.data_dir, &segments) else {
        return not_found().into_response();
    };
    if rest != ["settings"] {
        return not_found().into_response();
    }
    save_settings(state, &repo, &rel, cookie, body).await
}

/// Apply a settings edit, then redirect back to the settings page on success or
/// render the reason it was rejected.
async fn save_settings(
    state: &AppState,
    repo: &Path,
    rel: &str,
    cookie: Option<&str>,
    body: Bytes,
) -> Response {
    let (Some(seed), Some(hooks)) = (state.cert_nonce_seed.clone(), state.hooks_dir.clone()) else {
        return edit_error(
            rel,
            "Editing is disabled: this server is not enforcing the signed-push gate.",
        )
        .into_response();
    };
    let description = write::field(&body, "description").unwrap_or_default();

    let sessions = state.sessions.clone();
    let cookie = cookie.map(str::to_owned);
    let repo = repo.to_owned();
    let result = tokio::task::spawn_blocking(move || {
        write::edit_description(
            &sessions,
            cookie.as_deref(),
            &repo,
            &description,
            &seed,
            &hooks,
        )
    })
    .await;

    match result {
        Ok(Ok(())) => redirect(&format!("/{rel}/settings"), None),
        Ok(Err(error)) => edit_error(rel, &error).into_response(),
        Err(_join) => edit_error(rel, "the edit did not complete").into_response(),
    }
}

/// A `303 See Other` redirect to `location`, optionally setting a cookie.
fn redirect(location: &str, set_cookie: Option<String>) -> Response {
    let mut builder = Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header(LOCATION, location);
    if let Some(cookie) = set_cookie {
        builder = builder.header(SET_COOKIE, cookie);
    }
    builder
        .body(axum::body::Body::empty())
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// The `Set-Cookie` value that opens a session.
fn session_cookie(token: &str) -> String {
    format!("{}={token}; Path=/; HttpOnly; SameSite=Lax", write::COOKIE)
}

/// The `Set-Cookie` value that clears a session.
fn cleared_cookie() -> String {
    format!(
        "{}=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax",
        write::COOKIE
    )
}

/// Dispatch the part of the path that follows the repository to a browse view.
/// Each top-level tab is its own route, since the product is server-rendered
/// with no client JavaScript.
async fn route(
    repo: &Path,
    rel: &str,
    rest: &[&str],
    host: Option<&str>,
    session: Option<write::SessionSnapshot>,
) -> Response {
    let meta = gather_meta(repo, rel).await;
    match rest.split_first() {
        None => pages::repo_page(repo, &meta, host).await.into_response(),
        Some((&"files", sub)) => pages::files_page(repo, &meta, sub).await,
        Some((&"tree", sub)) => pages::tree_page(repo, &meta, sub).await,
        Some((&"blob", sub)) => pages::blob_page(repo, &meta, sub).await,
        Some((&"commit", &[sha])) => pages::commit_page(repo, &meta, sha).await,
        Some((&"releases", &[])) => pages::releases_page(repo, &meta).await.into_response(),
        Some((&"checks", &[])) => pages::checks_page(repo, &meta).await.into_response(),
        Some((&"issues", &[])) => pages::issues_page(repo, &meta).await.into_response(),
        Some((&"settings", &[])) => {
            let auth = resolve_auth(repo, session).await;
            pages::settings_page(repo, &meta, auth.as_ref())
                .await
                .into_response()
        }
        _ => not_found().into_response(),
    }
}

/// Resolve the request's session into per-repo [`Auth`]: whether the session's
/// web key is a member of `repo`, and under which username.
async fn resolve_auth(repo: &Path, session: Option<write::SessionSnapshot>) -> Option<Auth> {
    let session = session?;
    let repo = repo.to_owned();
    let key = session.public_key.clone();
    let username = tokio::task::spawn_blocking(move || write::member_for_public_key(&repo, &key))
        .await
        .ok()
        .flatten();
    Some(Auth {
        label: session.label,
        username,
    })
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
    homepage: Option<String>,
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
    let config = load_config(repo).await.unwrap_or_default();
    let description = Some(config.description.trim().to_owned()).filter(|s| !s.is_empty());
    let homepage = Some(config.homepage.trim().to_owned()).filter(|s| !s.is_empty());
    let topics = config
        .topics
        .into_iter()
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty())
        .collect();
    let releases = git_output(repo, &["tag", "--list"])
        .await
        .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count())
        .unwrap_or(0);
    let issues = open_issue_count(repo).await;
    RepoMeta {
        rel: rel.to_owned(),
        branch,
        description,
        homepage,
        topics,
        releases,
        issues,
    }
}

/// Load the repository's `refs/meta/config` document off the async runtime.
async fn load_config(repo: &Path) -> Option<git_ents::config::Config> {
    let repo = repo.to_owned();
    tokio::task::spawn_blocking(move || git_ents::config::load(&repo))
        .await
        .ok()?
        .ok()
}

/// Count the repository's open issues off the async runtime.
async fn open_issue_count(repo: &Path) -> usize {
    let repo = repo.to_owned();
    tokio::task::spawn_blocking(move || git_ents::issues::open_count(&repo))
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or(0)
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
fn index(state: &AppState, session: Option<&write::SessionSnapshot>) -> Markup {
    let repos = discover_repos(&state.data_dir);
    page(
        "Repositories",
        html! {
            (account_strip(session))
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

/// A small right-aligned strip showing who is signed in, with a sign-in or
/// sign-out control.
fn account_strip(session: Option<&write::SessionSnapshot>) -> Markup {
    html! {
        div.account-strip {
            @match session {
                Some(s) => {
                    span.muted { "Signed in · " (s.label) }
                    form method="post" action="/logout" {
                        button.btn.btn-quiet type="submit" { "Sign out" }
                    }
                }
                None => a.btn.btn-quiet href="/login" { "Sign in" }
            }
        }
    }
}

/// The sign-in page: paste a web key to open a session. `error` shows a failed
/// attempt's reason.
fn login_page(session: Option<&write::SessionSnapshot>, error: Option<&str>) -> Markup {
    page(
        "Sign in",
        html! {
            (account_strip(session))
            div.page-header { h1.page-title { "Sign in" } }
            @if let Some(s) = session {
                p { "Signed in as " strong { (s.label) } "." }
                p.muted { "Your web key signs edits made in the browser." }
            } @else {
                p.shell-note {
                    "Paste the " strong { "private" } " half of a web key whose public half you "
                    "have added to your member ref. It is held in memory for this session only "
                    "and is never written to disk."
                }
                @if let Some(error) = error {
                    div.card-row.muted { "Could not sign in: " (error) }
                }
                form.edit-form method="post" action="/login" {
                    label { "Key name (optional)" }
                    input type="text" name="label" placeholder="laptop web key";
                    label { "Private key" }
                    textarea name="private_key" rows="8" spellcheck="false"
                        placeholder="-----BEGIN OPENSSH PRIVATE KEY-----" {}
                    button.btn type="submit" { "Sign in" }
                }
            }
        },
    )
}

/// A page reporting why a settings edit was rejected, with a way back.
fn edit_error(rel: &str, error: &str) -> Markup {
    page(
        "Edit rejected",
        html! {
            div.page-header { h1.page-title { "Edit rejected" } }
            div.card-row.muted { (error) }
            p { a.btn href={ "/" (rel) "/settings" } { "Back to settings" } }
        },
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
