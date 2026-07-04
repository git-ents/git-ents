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
mod debug;
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

pub(crate) use self::debug::handshake;
pub(crate) use self::write::{Challenges, Sessions, new_challenges, new_sessions};

/// Who is signed in for the current request, resolved per repository: a member's
/// web key authorizes edits only on a repo whose member list contains it.
pub(super) struct Auth {
    /// The session key's display label.
    label: String,
    /// The member username this key maps to in the current repo, when it is a
    /// member there — the gate for showing edit controls.
    username: Option<String>,
    /// The session's CSRF token, echoed in edit forms.
    csrf: String,
}

use self::assets::{COPY_SCRIPT, FONTS, LIVE_SCRIPT, STYLE};
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
        let challenge = match session {
            Some(_) => None,
            None => write::issue_challenge(&state.challenges).ok(),
        };
        return login_page(session.as_ref(), challenge.as_deref(), None).into_response();
    }
    // The CLI signs in the same way the browser form does, just without the
    // HTML: a bare nonce to sign, and (via `handle_post`) a bare token back.
    if segments == ["login", "cli"] {
        return match write::issue_challenge(&state.challenges) {
            Ok(nonce) => nonce.into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        };
    }

    if let Some((repo, rel, rest)) = resolve_repo(&state.data_dir, &segments) {
        return route(
            &repo,
            &rel,
            rest,
            host,
            session,
            editing_enabled(state),
            &state.live_runs,
        )
        .await;
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
        // Bare repos are stored on disk as `<name>.git`, matching what
        // `git http-backend` expects, but the friendly web/CLI paths never
        // include the suffix — so try it on the last segment before giving up.
        let repo = if is_bare_repo(&repo) {
            repo
        } else if let Some((last, init)) = repo_segs.split_last() {
            let mut with_suffix = data_dir.join(init.iter().collect::<PathBuf>());
            with_suffix.push(format!("{last}.git"));
            if is_bare_repo(&with_suffix) {
                with_suffix
            } else {
                continue;
            }
        } else {
            continue;
        };
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
    let secure = is_secure_request(headers);
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    if segments == ["login"] {
        return match write::login(&state.sessions, &state.challenges, &body) {
            Ok(token) => redirect("/login", Some(session_cookie(&token, secure))),
            Err(error) => {
                let challenge = write::issue_challenge(&state.challenges).ok();
                login_page(None, challenge.as_deref(), Some(&error)).into_response()
            }
        };
    }
    if segments == ["login", "cli"] {
        return match write::login(&state.sessions, &state.challenges, &body) {
            Ok(token) => token.into_response(),
            Err(error) => (StatusCode::UNAUTHORIZED, error).into_response(),
        };
    }
    if segments == ["logout"] {
        // A cross-site form cannot read the session's CSRF token, so an absent
        // or wrong one means the request did not originate from our own page.
        if !write::csrf_ok(
            &state.sessions,
            cookie,
            &write::field(&body, "csrf").unwrap_or_default(),
        ) {
            return redirect("/login", None);
        }
        write::logout(&state.sessions, cookie);
        return redirect("/login", Some(cleared_cookie(secure)));
    }

    let Some((repo, rel, rest)) = resolve_repo(&state.data_dir, &segments) else {
        return not_found().into_response();
    };
    match rest {
        ["settings"] => save_settings(state, &repo, &rel, cookie, body).await,
        ["comment"] => save_comment(state, &repo, &rel, cookie, body).await,
        _ => not_found().into_response(),
    }
}

/// Whether this server can actually land browser edits: it needs the signed-push
/// gate (nonce seed + hooks) and its own signing key, all of which
/// [`write::edit_config`] requires. When any is unset, edit controls are not
/// offered so a member is never told they can edit when a submit would only fail.
fn editing_enabled(state: &AppState) -> bool {
    state.cert_nonce_seed.is_some() && state.hooks_dir.is_some() && state.web_signing_key.is_some()
}

/// Whether the request reached us over HTTPS — directly, or through a TLS
/// terminator that set `X-Forwarded-Proto`. Gates the cookie `Secure` flag so a
/// plain-HTTP development server still works.
fn is_secure_request(headers: &HeaderMap) -> bool {
    headers
        .get("X-Forwarded-Proto")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|proto| proto.eq_ignore_ascii_case("https"))
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
    let (Some(seed), Some(hooks), Some(signing_key)) = (
        state.cert_nonce_seed.clone(),
        state.hooks_dir.clone(),
        state.web_signing_key.clone(),
    ) else {
        return edit_error(
            &format!("/{rel}/settings"),
            "Editing is disabled: this server has no web signing key or signed-push gate.",
        )
        .into_response();
    };
    if !write::csrf_ok(
        &state.sessions,
        cookie,
        &write::field(&body, "csrf").unwrap_or_default(),
    ) {
        return edit_error(
            &format!("/{rel}/settings"),
            "the edit could not be verified; reload and try again",
        )
        .into_response();
    }
    let edit = write::ConfigEdit {
        description: write::field(&body, "description").unwrap_or_default(),
        homepage: write::field(&body, "homepage").unwrap_or_default(),
        topics: write::field(&body, "topics")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|topic| !topic.is_empty())
            .map(str::to_owned)
            .collect(),
    };

    let sessions = state.sessions.clone();
    let cookie = cookie.map(str::to_owned);
    let repo = repo.to_owned();
    let result = tokio::task::spawn_blocking(move || {
        write::edit_config(
            &sessions,
            cookie.as_deref(),
            &repo,
            &edit,
            &seed,
            &hooks,
            &signing_key,
        )
    })
    .await;

    let back = format!("/{rel}/settings");
    match result {
        Ok(Ok(())) => redirect(&back, None),
        Ok(Err(error)) => edit_error(&back, &error).into_response(),
        Err(_join) => edit_error(&back, "the edit did not complete").into_response(),
    }
}

/// Record a code comment posted from a file view, then redirect back to that
/// file on success or render the reason it was rejected.
async fn save_comment(
    state: &AppState,
    repo: &Path,
    rel: &str,
    cookie: Option<&str>,
    body: Bytes,
) -> Response {
    let path = write::field(&body, "path").unwrap_or_default();
    let back = format!("/{rel}/blob/{path}");
    let (Some(seed), Some(hooks), Some(signing_key)) = (
        state.cert_nonce_seed.clone(),
        state.hooks_dir.clone(),
        state.web_signing_key.clone(),
    ) else {
        return edit_error(
            &back,
            "Editing is disabled: this server has no web signing key or signed-push gate.",
        )
        .into_response();
    };
    if !write::csrf_ok(
        &state.sessions,
        cookie,
        &write::field(&body, "csrf").unwrap_or_default(),
    ) {
        return edit_error(
            &back,
            "the comment could not be verified; reload and try again",
        )
        .into_response();
    }
    let lines = match write::parse_lines(&write::field(&body, "lines").unwrap_or_default()) {
        Ok(lines) => lines,
        Err(error) => return edit_error(&back, &error).into_response(),
    };
    let text = write::field(&body, "body")
        .unwrap_or_default()
        .trim()
        .to_owned();
    if path.is_empty() || text.is_empty() {
        return edit_error(&back, "a comment needs a file path and a body").into_response();
    }
    let edit = write::CommentEdit {
        path,
        lines,
        body: text,
    };

    let sessions = state.sessions.clone();
    let cookie = cookie.map(str::to_owned);
    let repo = repo.to_owned();
    let result = tokio::task::spawn_blocking(move || {
        write::add_comment(
            &sessions,
            cookie.as_deref(),
            &repo,
            &edit,
            &seed,
            &hooks,
            &signing_key,
        )
    })
    .await;

    match result {
        Ok(Ok(())) => redirect(&back, None),
        Ok(Err(error)) => edit_error(&back, &error).into_response(),
        Err(_join) => edit_error(&back, "the comment did not complete").into_response(),
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

/// The `Set-Cookie` value that opens a session, marked `Secure` over HTTPS.
fn session_cookie(token: &str, secure: bool) -> String {
    format!(
        "{}={token}; Path=/; HttpOnly; SameSite=Lax{}",
        write::COOKIE,
        if secure { "; Secure" } else { "" }
    )
}

/// The `Set-Cookie` value that clears a session.
fn cleared_cookie(secure: bool) -> String {
    format!(
        "{}=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax{}",
        write::COOKIE,
        if secure { "; Secure" } else { "" }
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
    editing: bool,
    live_runs: &crate::checks::LiveRegistry,
) -> Response {
    let meta = gather_meta(repo, rel).await;
    match rest.split_first() {
        None => pages::repo_page(repo, &meta, host).await.into_response(),
        Some((&"files", sub)) => {
            let auth = resolve_auth(repo, session).await;
            pages::files_page(repo, &meta, sub, auth.as_ref(), editing).await
        }
        Some((&"tree", sub)) => pages::tree_page(repo, &meta, sub).await,
        Some((&"blob", sub)) => {
            let auth = resolve_auth(repo, session).await;
            pages::blob_page(
                repo,
                &meta,
                sub,
                auth.as_ref(),
                editing,
                pages::BlobView::Rendered,
            )
            .await
        }
        Some((&"source", sub)) => {
            let auth = resolve_auth(repo, session).await;
            pages::blob_page(
                repo,
                &meta,
                sub,
                auth.as_ref(),
                editing,
                pages::BlobView::Source,
            )
            .await
        }
        Some((&"commit", &[sha])) => pages::commit_page(repo, &meta, sha).await,
        Some((&"releases", &[])) => pages::releases_page(repo, &meta).await.into_response(),
        Some((&"checks", &[])) => pages::checks_page(repo, &meta).await.into_response(),
        Some((&"checks", &[commit, name])) => {
            pages::check_recording_page(repo, &meta, commit, name, live_runs).await
        }
        Some((&"checks", &[commit, name, "live"])) => {
            pages::check_live_fragment(repo, commit, name, live_runs).await
        }
        Some((&"issues", &[])) => pages::issues_page(repo, &meta).await.into_response(),
        Some((&"settings", &[])) => {
            let auth = resolve_auth(repo, session).await;
            pages::settings_page(repo, &meta, auth.as_ref(), editing)
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
        csrf: session.csrf,
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
                        input type="hidden" name="csrf" value=(s.csrf);
                        button.btn.btn-quiet type="submit" { "Sign out" }
                    }
                }
                None => a.btn.btn-quiet href="/login" { "Sign in" }
            }
        }
    }
}

/// The sign-in page: prove control of a member key by signing a one-time
/// challenge locally, without ever surrendering the key. `error` shows a failed
/// attempt's reason; `challenge` is the nonce to sign.
fn login_page(
    session: Option<&write::SessionSnapshot>,
    challenge: Option<&str>,
    error: Option<&str>,
) -> Markup {
    page(
        "Sign in",
        html! {
            (account_strip(session))
            div.page-header { h1.page-title { "Sign in" } }
            @if let Some(s) = session {
                p { "Signed in as " strong { (s.label) } "." }
                p.muted { "Edits you make in the browser are attributed to your member key." }
            } @else {
                p.shell-note {
                    "Prove control of a web key whose public half is a member of the repository. "
                    "Sign the one-time challenge below on your own machine — the key never leaves it."
                }
                @if let Some(error) = error {
                    div.card-row.muted { "Could not sign in: " (error) }
                }
                @if let Some(nonce) = challenge {
                    p.shell-note {
                        "Have " code { "git-ents" } " installed? Run "
                        code { "git ents login <remote>" }
                        " instead."
                    }
                    p.shell-note { "Otherwise, run this, then paste the output and your public key:" }
                    pre.signin-cmd {
                        "printf %s '" (nonce) "' | ssh-keygen -Y sign -n "
                        (write::LOGIN_NAMESPACE) " -f ~/.ssh/your_web_key"
                    }
                    form.edit-form method="post" action="/login" {
                        input type="hidden" name="nonce" value=(nonce);
                        label { "Public key" }
                        input type="text" name="public_key" spellcheck="false"
                            placeholder="ssh-ed25519 AAAA… you@host";
                        label { "Signature" }
                        textarea name="signature" rows="8" spellcheck="false"
                            placeholder="-----BEGIN SSH SIGNATURE-----" {}
                        button.btn type="submit" { "Sign in" }
                    }
                } @else {
                    div.card-row.muted { "Could not start a sign-in challenge; reload to retry." }
                }
            }
        },
    )
}

/// A page reporting why a browser write was rejected, with a way back to the
/// page it was posted from.
fn edit_error(back: &str, error: &str) -> Markup {
    page(
        "Edit rejected",
        html! {
            div.page-header { h1.page-title { "Edit rejected" } }
            div.card-row.muted { (error) }
            p { a.btn href=(back) { "Back" } }
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
                script { (PreEscaped(LIVE_SCRIPT)) }
            }
        }
    }
}
