//! Integration coverage for `docs/spec/roots.adoc`'s web-frontend
//! requirements, driven entirely through [`tower::ServiceExt::oneshot`]
//! against [`ents_web::router`] -- no socket is ever bound anywhere in
//! this file, which is itself part of the proof for `roots.web-agnostic`:
//! every one of these requests is exercised the same way an in-process
//! webview embedding would drive them.
#![allow(clippy::expect_used, reason = "integration test")]
#![allow(clippy::unwrap_used, reason = "integration test")]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use ents_kiln::Toolchain;
use ents_model::{Account, Effect, MemberId, Provenance, Redaction};
use ents_receive::{Mode, NullEventSink};
use ents_testutil::{
    CommitSpec, Keypair, MemRefStore, ObjectStore, enroll_member, write_commit, write_meta_entity,
};
use ents_web::identity::SigningIdentity;
use ents_web::state::AppState;
use gix::bstr::ByteSlice as _;
use gix_object::tree::{Entry, EntryKind};
use gix_object::{Kind, Tree, Write as _};
use http_body_util::BodyExt as _;
use tower::ServiceExt as _;

/// A fixture [`SigningIdentity`] wrapping a deterministic test key, named
/// so a test can tell two different injected identities apart by their
/// commit author name alone.
struct FixtureIdentity {
    name: &'static str,
    key: Keypair,
}

impl SigningIdentity for FixtureIdentity {
    fn actor(&self) -> gix::actor::Signature {
        gix::actor::Signature {
            name: self.name.into(),
            email: format!("{}@ents.test", self.name).into(),
            time: gix::date::Time {
                seconds: 1_000,
                offset: 0,
            },
        }
    }

    fn sign(&self, payload: &[u8]) -> String {
        self.key.sign(payload)
    }

    fn public_openssh(&self) -> String {
        self.key.public_openssh()
    }
}

fn build_state(identity: FixtureIdentity) -> Arc<AppState<ObjectStore>> {
    Arc::new(AppState::new(
        Box::new(MemRefStore::default()),
        ObjectStore::default(),
        Box::new(NullEventSink),
        Mode::Advisory,
        Box::new(identity),
        std::env::temp_dir(),
    ))
}

/// Like [`build_state`], but `path` names a real, on-disk repository
/// rather than the shared system temp directory -- `crate::pages::files`
/// opens `state.path` directly with `gix::open`, so its tests need an
/// actual `HEAD` to browse, not just the in-memory ref/object store every
/// other test in this file exercises.
fn build_state_at(
    identity: FixtureIdentity,
    path: std::path::PathBuf,
) -> Arc<AppState<ObjectStore>> {
    Arc::new(AppState::new(
        Box::new(MemRefStore::default()),
        ObjectStore::default(),
        Box::new(NullEventSink),
        Mode::Advisory,
        Box::new(identity),
        path,
    ))
}

/// Like [`build_state`], but `refs`/`objects` are already populated --
/// what the toolchain-marker tests below use to seed a ref store directly
/// with plain `gix_object` writes (a wrong-shape tree no `ents-kiln`
/// helper would ever produce), rather than through a signed write path.
fn build_state_with(
    identity: FixtureIdentity,
    refs: MemRefStore,
    objects: ObjectStore,
) -> Arc<AppState<ObjectStore>> {
    Arc::new(AppState::new(
        Box::new(refs),
        objects,
        Box::new(NullEventSink),
        Mode::Advisory,
        Box::new(identity),
        std::env::temp_dir(),
    ))
}

/// Land a `refs/meta/toolchains/<name>` ref pointing at a tree shaped like
/// the pre-redo `git_toolchain::Bin` schema this repository's own
/// `refs/meta/toolchains/{rust,sccache,zig}` still carry: a `recipe` entry
/// that is itself a tree, not the blob today's `ents_kiln::Toolchain::recipe:
/// String` expects -- `facet_git_tree::deserialize` reads `recipe` as a
/// scalar (a blob) and fails with `NotABlob` on exactly this shape, the
/// same failure `git ents serve` hits reading this repository's own real
/// legacy toolchain refs (piece 1's bug report). Built from plain
/// `gix_object` writes, not `ents_kiln::toolchain::import` (which only ever
/// writes today's shape) or `write_meta_entity` (which only ever writes a
/// value that already round-trips through `facet_git_tree`).
fn write_legacy_toolchain(refs: &MemRefStore, objects: &ObjectStore, name: &str) {
    let name_blob = objects
        .write_buf(Kind::Blob, name.as_bytes())
        .expect("write");
    let recipe_tree = objects
        .write(&Tree {
            entries: Vec::new(),
        })
        .expect("write");
    let mut entries = vec![
        Entry {
            mode: EntryKind::Blob.into(),
            filename: "name".into(),
            oid: name_blob,
        },
        Entry {
            mode: EntryKind::Tree.into(),
            filename: "recipe".into(),
            oid: recipe_tree,
        },
    ];
    entries.sort();
    let tree = objects.write(&Tree { entries }).expect("write");
    let tip = write_commit(
        objects,
        &CommitSpec {
            tree,
            parents: Vec::new(),
            message: format!("legacy toolchain {name}"),
            seconds: 100,
        },
        None,
    );
    let refname: gix::refs::FullName = format!("refs/meta/toolchains/{name}")
        .try_into()
        .expect("valid refname");
    refs.set(refname.as_ref(), tip);
}

/// Initialize a real git repository at a fresh tempdir, seed it with
/// `files` (path, contents), and commit them on `HEAD` -- what
/// `crate::pages::files`'s tests below browse.
fn seed_repo(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(args)
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?} failed");
    };
    git(&["init", "-q"]);
    for (name, contents) in files {
        let path = dir.path().join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir -p");
        }
        std::fs::write(&path, contents).expect("write fixture file");
    }
    git(&["add", "-A"]);
    git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@example.com",
        "commit",
        "-q",
        "-m",
        "seed",
    ]);
    dir
}

/// The full hex object id of `dir`'s current `HEAD` commit, read via `git
/// rev-parse` -- what `crate::pages::commits`'s tests below build
/// `/commit/{oid}` request paths from.
fn head_oid(dir: &std::path::Path) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("git runs");
    assert!(output.status.success(), "git rev-parse HEAD failed");
    String::from_utf8(output.stdout)
        .expect("utf8 oid")
        .trim()
        .to_owned()
}

/// Commit a further change to a file already tracked in `dir` -- what the
/// comment tests below use to move `HEAD` past a comment's own anchor
/// commit, so its projection has something to react to.
fn commit_change(dir: &std::path::Path, path: &str, contents: &str, message: &str) {
    std::fs::write(dir.join(path), contents).expect("write fixture file");
    let git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?} failed");
    };
    git(&["add", "-A"]);
    git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@example.com",
        "commit",
        "-q",
        "-m",
        message,
    ]);
}

/// `GET path` and return its body decoded as UTF-8, asserting a 200 --
/// the read-back half of the many "mutate, then observe" tests below, so
/// each does not re-spell the collect/decode dance inline.
async fn get_body(router: &axum::Router, path: &str) -> String {
    let response = router
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).expect("request"))
        .await
        .expect("in-process call");
    assert_eq!(response.status(), StatusCode::OK, "GET {path}");
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    String::from_utf8(bytes.to_vec()).expect("utf8 html")
}

/// Establish a session against `router` via a `GET` to `path`, returning
/// its cookie header and CSRF token -- the same extraction
/// `csrf_is_required_and_checked_on_every_state_changing_request` performs
/// inline, factored out here since every comment test below needs one.
async fn session_cookie_and_csrf(
    router: &axum::Router,
    state: &AppState<ObjectStore>,
    path: &str,
) -> (String, String) {
    let response = router
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).expect("request"))
        .await
        .expect("in-process call");
    let cookie = response
        .headers()
        .get(header::SET_COOKIE)
        .expect("a fresh GET always mints a session cookie")
        .to_str()
        .expect("ascii")
        .to_owned();
    let session_id = cookie
        .split(';')
        .next()
        .expect("at least one segment")
        .split_once('=')
        .expect("name=value")
        .1
        .to_owned();
    let csrf = state
        .sessions
        .get(&session_id)
        .expect("the session this cookie names is held in this server's own memory")
        .csrf;
    (cookie, csrf)
}

/// `POST /comments`, anchoring `body` to `path` (`lines`, `<start>:<end>`)
/// at `rev` -- what the comment tests below seed a real comment through,
/// exercising the actual signed-write path (`ents_forge::comment::add`)
/// rather than poking the ref store directly. Asserts the write succeeded
/// (a redirect to the new comment's own page) and returns the new comment's
/// id, read from that redirect's `Location` (`/comments/<id>`) -- what the
/// thread-action tests below drive reply/resolve/reopen against.
async fn seed_comment(
    router: &axum::Router,
    state: &AppState<ObjectStore>,
    path: &str,
    body: &str,
    lines: &str,
    rev: &str,
) -> String {
    let (cookie, csrf) = session_cookie_and_csrf(router, state, "/comments").await;
    let form = format!(
        "path={path}&body={}&lines={lines}&rev={rev}&csrf={csrf}",
        body.replace(' ', "+")
    );
    let response = router
        .clone()
        .oneshot(
            Request::post("/comments")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::COOKIE, cookie)
                .body(Body::from(form))
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert!(
        response.status().is_redirection(),
        "comment write did not succeed: {:?}",
        response.status()
    );
    response
        .headers()
        .get(header::LOCATION)
        .expect("a successful comment write redirects to the new comment")
        .to_str()
        .expect("ascii")
        .strip_prefix("/comments/")
        .expect("redirect targets /comments/<id>")
        .to_owned()
}

/// `roots.local`: this crate's route table never exposes git's own
/// smart-HTTP transport -- a request that would name it (`info/refs` with
/// a `service` query, exactly the URL stock `git clone`/`git fetch` sends
/// a dumb or smart HTTP backend) falls through to axum's ordinary 404,
/// not a git wire-protocol response.
#[tokio::test]
// @relation(roots.local, scope=function, role=Verifies)
async fn smart_http_transport_is_never_exposed() {
    let state = build_state(FixtureIdentity {
        name: "local-user",
        key: Keypair::from_seed(1),
    });
    let router = ents_web::router(state);

    let response = router
        .oneshot(
            Request::get("/info/refs?service=git-upload-pack")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// `GET /style.css` is reachable with no session at all (every other route
/// runs behind the session middleware, but that middleware only ever
/// attaches a session -- it never gates), and serves this crate's own
/// hand-rolled stylesheet.
#[tokio::test]
async fn style_css_is_served_with_no_session_required() {
    let state = build_state(FixtureIdentity {
        name: "local-user",
        key: Keypair::from_seed(1),
    });
    let router = ents_web::router(state);

    let response = router
        .oneshot(
            Request::get("/style.css")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .expect("content-type")
            .to_str()
            .expect("ascii"),
        "text/css; charset=utf-8"
    );
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body = String::from_utf8(body.to_vec()).expect("utf8 css");
    assert!(!body.is_empty());
    assert!(body.contains("--color-bg"));
}

/// `roots.web-agnostic`: the dashboard actually renders real content
/// in-process, with no socket bound anywhere in this test -- reading the
/// body back (rather than only checking the status) is what makes this
/// test more than a routing smoke check.
#[tokio::test]
// @relation(roots.web-agnostic, scope=function, role=Verifies)
async fn dashboard_renders_in_process_with_no_socket_bound() {
    let state = build_state(FixtureIdentity {
        name: "local-user",
        key: Keypair::from_seed(1),
    });
    let router = ents_web::router(state);

    let response = router
        .oneshot(Request::get("/").body(Body::empty()).expect("request"))
        .await
        .expect("in-process call");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body = String::from_utf8(body.to_vec()).expect("utf8 html");
    assert!(body.contains("members"));
    assert!(body.contains("toolchains"));
    // The shell chrome renders on every page: the icon rail, the sticky
    // top bar, and the bar's palette search form.
    assert!(body.contains("class=\"rail\""));
    assert!(body.contains("class=\"wb-bar\""));
    assert!(body.contains("class=\"palette\""));
    assert!(body.contains("Jump to file, commit, ticket, member"));
}

/// `roots.web-agnostic`: the shell's `.wb-bar` top bar names the served
/// repository (its directory name) and, when `HEAD` resolves to a branch,
/// renders that branch in the `.branch` pill -- both read once off
/// `AppState.path`, so every page's chrome reflects the actual repository
/// being served rather than a placeholder.
#[tokio::test]
async fn the_top_bar_names_the_served_repo_and_its_head_branch() {
    let dir = seed_repo(&[("README.md", "# hi\n")]);
    // `git init` picks the default branch name (which varies by host git
    // config); rename it so the pill's text is deterministic to assert.
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .args(["branch", "-m", "trunk"])
        .status()
        .expect("git runs");
    assert!(status.success(), "git branch -m failed");
    let repo_name = dir
        .path()
        .file_name()
        .expect("tempdir has a name")
        .to_string_lossy()
        .into_owned();

    let state = build_state_at(
        FixtureIdentity {
            name: "local-user",
            key: Keypair::from_seed(1),
        },
        dir.path().to_owned(),
    );
    let router = ents_web::router(state);

    let response = router
        .oneshot(Request::get("/").body(Body::empty()).expect("request"))
        .await
        .expect("in-process call");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body = String::from_utf8(body.to_vec()).expect("utf8 html");
    assert!(body.contains("class=\"wb-bar\""));
    assert!(
        body.contains(&repo_name),
        "the served repo's directory name {repo_name:?} must appear in the top bar"
    );
    assert!(
        body.contains("class=\"branch\""),
        "a resolvable HEAD must render the branch pill"
    );
    assert!(
        body.contains("trunk"),
        "the pill carries the short branch name"
    );
}

/// `roots.web-agnostic`: the overview (`GET /`) renders the served
/// repository's `README` as HTML in its main column and a language
/// breakdown of the `HEAD` tree in its aside -- both read off `state.path`
/// with `gix`, so the dashboard reflects real repository content, not just
/// the meta-ref counts.
#[tokio::test]
async fn dashboard_renders_the_readme_and_a_languages_card() {
    let dir = seed_repo(&[
        ("README.md", "# Welcome\n\nThe project overview.\n"),
        ("src/main.rs", "fn main() {}\n"),
        ("src/lib.rs", "pub fn f() {}\n"),
    ]);
    let state = build_state_at(
        FixtureIdentity {
            name: "local-user",
            key: Keypair::from_seed(1),
        },
        dir.path().to_owned(),
    );
    let router = ents_web::router(state);

    let response = router
        .oneshot(Request::get("/").body(Body::empty()).expect("request"))
        .await
        .expect("in-process call");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body = String::from_utf8(body.to_vec()).expect("utf8 html");
    assert!(
        body.contains("class=\"overview\""),
        "the overview grid renders"
    );
    assert!(
        body.contains("<h1>Welcome</h1>"),
        "the README renders as HTML, not raw markdown"
    );
    assert!(
        body.contains("lang-bar") && body.contains("Rust"),
        "the language breakdown names the tree's languages"
    );
}

/// `GET /` shows a freshness strip above the `README` card: the `HEAD`
/// commit's own subject and a link into its `/commit/{oid}` page.
#[tokio::test]
async fn dashboard_shows_a_freshness_strip_linking_to_the_latest_commit() {
    let dir = seed_repo(&[("README.md", "# hi\n")]);
    let oid = head_oid(dir.path());
    let state = build_state_at(
        FixtureIdentity {
            name: "local-user",
            key: Keypair::from_seed(1),
        },
        dir.path().to_owned(),
    );
    let router = ents_web::router(state);

    let response = router
        .oneshot(Request::get("/").body(Body::empty()).expect("request"))
        .await
        .expect("in-process call");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body = String::from_utf8(body.to_vec()).expect("utf8 html");
    assert!(body.contains("class=\"card freshness\""));
    assert!(body.contains(&format!("/commit/{oid}")));
    assert!(body.contains("seed"), "the HEAD commit's subject renders");
}

/// `GET /` on an unborn `HEAD` (a freshly initialized, still-empty
/// repository) omits the freshness strip entirely, rather than rendering
/// a placeholder for a commit that does not exist.
#[tokio::test]
async fn dashboard_omits_the_freshness_strip_on_an_unborn_head() {
    let dir = tempfile::tempdir().expect("tempdir");
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .args(["init", "-q"])
        .status()
        .expect("git runs");
    assert!(status.success(), "git init failed");
    let state = build_state_at(
        FixtureIdentity {
            name: "local-user",
            key: Keypair::from_seed(1),
        },
        dir.path().to_owned(),
    );
    let router = ents_web::router(state);

    let response = router
        .oneshot(Request::get("/").body(Body::empty()).expect("request"))
        .await
        .expect("in-process call");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body = String::from_utf8(body.to_vec()).expect("utf8 html");
    assert!(!body.contains("class=\"card freshness\""));
}

/// `roots.web-session`: a state-changing request with no CSRF token at
/// all is a bad request (axum's own `Form` rejection); one with the wrong
/// token is refused by this crate's own check; the session cookie a `GET`
/// mints is required to learn the right one at all.
#[tokio::test]
// @relation(roots.web-session, scope=function, role=Verifies)
async fn csrf_is_required_and_checked_on_every_state_changing_request() {
    let state = build_state(FixtureIdentity {
        name: "local-user",
        key: Keypair::from_seed(1),
    });
    let router = ents_web::router(Arc::clone(&state));

    // No CSRF field at all in the POST body.
    let response = router
        .clone()
        .oneshot(
            Request::post("/account")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("member=jdc&login=jdc@ents.test"))
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert!(
        !response.status().is_success(),
        "a POST with no csrf field at all must not succeed"
    );

    // A GET establishes a session; extract its id from Set-Cookie, then
    // read the matching CSRF token directly out of the (in-memory-only)
    // session store this test built.
    let get_response = router
        .clone()
        .oneshot(
            Request::get("/account")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("in-process call");
    let cookie = get_response
        .headers()
        .get(header::SET_COOKIE)
        .expect("a fresh GET always mints a session cookie")
        .to_str()
        .expect("ascii")
        .to_owned();
    let session_id = cookie
        .split(';')
        .next()
        .expect("at least one segment")
        .split_once('=')
        .expect("name=value")
        .1
        .to_owned();
    let csrf = state
        .sessions
        .get(&session_id)
        .expect("the session this cookie names is held in this server's own memory")
        .csrf;

    // The wrong token is refused.
    let response = router
        .clone()
        .oneshot(
            Request::post("/account")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::COOKIE, cookie.clone())
                .body(Body::from(
                    "member=jdc&login=jdc@ents.test&csrf=not-the-token",
                ))
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // The right token, carried by the same session cookie, succeeds.
    let response = router
        .clone()
        .oneshot(
            Request::post("/account")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::COOKIE, cookie)
                .body(Body::from(format!(
                    "member=jdc&login=jdc@ents.test&csrf={csrf}"
                )))
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert!(
        response.status().is_redirection(),
        "{:?}",
        response.status()
    );
}

/// `roots.web-session`: a session is recognized across requests that
/// carry its cookie (no fresh `Set-Cookie` reissued), and is held only in
/// this server's own process memory -- a second, independently built
/// state/router pair (standing in for a second process) never recognizes
/// a cookie the first one minted.
#[tokio::test]
// @relation(roots.web-session, scope=function, role=Verifies)
async fn a_session_is_recognized_across_requests_but_never_across_servers() {
    let state_a = build_state(FixtureIdentity {
        name: "a",
        key: Keypair::from_seed(1),
    });
    let router_a = ents_web::router(Arc::clone(&state_a));

    let first = router_a
        .clone()
        .oneshot(Request::get("/").body(Body::empty()).expect("request"))
        .await
        .expect("in-process call");
    let cookie = first
        .headers()
        .get(header::SET_COOKIE)
        .expect("first request mints a session")
        .clone();

    let second = router_a
        .clone()
        .oneshot(
            Request::get("/")
                .header(header::COOKIE, cookie.clone())
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert!(
        second.headers().get(header::SET_COOKIE).is_none(),
        "a recognized session must not be re-minted"
    );

    // A second server (fresh in-memory session store) never recognizes
    // the first server's cookie.
    let state_b = build_state(FixtureIdentity {
        name: "b",
        key: Keypair::from_seed(2),
    });
    let router_b = ents_web::router(state_b);
    let third = router_b
        .oneshot(
            Request::get("/")
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert!(
        third.headers().get(header::SET_COOKIE).is_some(),
        "a foreign session id must be treated as absent, minting a fresh one"
    );
}

/// `roots.web-signing`, `roots.web-agnostic`: the identical page handler,
/// reached through the identical route, signs each request's mutation
/// commit with whichever [`SigningIdentity`] its own composition root
/// injected -- never a fixed or shared one. This is the crate-level proof
/// the development plan assigns this phase; wiring an actual hosted
/// server-key identity behind `git-ents-server` is phase 8's job (see
/// this crate's own top-level doc).
#[tokio::test]
// @relation(roots.web-signing, roots.web-agnostic, scope=function, role=Verifies)
async fn each_request_is_signed_by_its_own_injected_identity_never_a_shared_one() {
    for (name, seed) in [("local-style", 11u8), ("hosted-style", 22u8)] {
        let state = build_state(FixtureIdentity {
            name,
            key: Keypair::from_seed(seed),
        });
        let router = ents_web::router(Arc::clone(&state));

        let get_response = router
            .clone()
            .oneshot(
                Request::get("/account")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("in-process call");
        let cookie = get_response
            .headers()
            .get(header::SET_COOKIE)
            .expect("session")
            .to_str()
            .expect("ascii")
            .to_owned();
        let session_id = cookie
            .split(';')
            .next()
            .expect("segment")
            .split_once('=')
            .expect("name=value")
            .1
            .to_owned();
        let csrf = state.sessions.get(&session_id).expect("session").csrf;

        let response = router
            .oneshot(
                Request::post("/account")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .header(header::COOKIE, cookie)
                    .body(Body::from(format!(
                        "member=jdc&login=jdc@ents.test&csrf={csrf}"
                    )))
                    .expect("request"),
            )
            .await
            .expect("in-process call");
        assert!(response.status().is_redirection());

        // Read the commit this request wrote back directly (this test's
        // own retained `state` handle, not a second connection) and
        // confirm its author is exactly this iteration's own identity.
        let name_ref: gix::refs::FullName = ents_model::namespace::ACCOUNT_REF
            .try_into()
            .expect("valid");
        let tip = state
            .refs
            .get(name_ref.as_ref())
            .expect("readable")
            .expect("account was just written");
        let mut buf = Vec::new();
        let objects = state.objects();
        let data = gix_object::Find::try_find(&*objects, &tip, &mut buf)
            .expect("read")
            .expect("present");
        let commit = gix_object::CommitRef::from_bytes(data.data, tip.kind()).expect("commit");
        assert!(
            commit.author.to_str_lossy().contains(name),
            "commit author {:?} must carry this iteration's own identity name {name:?}",
            commit.author.to_str_lossy()
        );

        let tree = commit.tree();
        let account: Account = facet_git_tree::deserialize(&tree, &*objects).expect("typed tree");
        assert_eq!(account.member, MemberId::new("jdc"));
    }
}

/// `GET /files` lists the served repository's root directory: every
/// top-level entry, directory or file, appears as a link.
#[tokio::test]
async fn files_root_lists_the_repository_root() {
    let dir = seed_repo(&[
        ("README.adoc", "= Welcome\n\nHello.\n"),
        ("docs/x.md", "# Doc Title\n\nSome text.\n"),
        ("src/main.rs", "fn main() {\n    let ok = 1 < 2;\n}\n"),
    ]);
    let state = build_state_at(
        FixtureIdentity {
            name: "local-user",
            key: Keypair::from_seed(1),
        },
        dir.path().to_owned(),
    );
    let router = ents_web::router(state);

    let response = router
        .oneshot(Request::get("/files").body(Body::empty()).expect("request"))
        .await
        .expect("in-process call");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body = String::from_utf8(body.to_vec()).expect("utf8 html");
    assert!(body.contains("README.adoc"));
    assert!(body.contains("docs"));
    assert!(body.contains("src"));
}

/// `GET /files/<path>` on a plain-text blob with no recognized grammar
/// renders a line-numbered, escaped `pre.blob-code` source view -- no
/// syntax highlighting, and no unescaped source.
#[tokio::test]
async fn files_blob_view_renders_a_plain_text_file() {
    let dir = seed_repo(&[("notes.txt", "true and 1 < 2\n")]);
    let state = build_state_at(
        FixtureIdentity {
            name: "local-user",
            key: Keypair::from_seed(1),
        },
        dir.path().to_owned(),
    );
    let router = ents_web::router(state);

    let response = router
        .oneshot(
            Request::get("/files/notes.txt")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body = String::from_utf8(body.to_vec()).expect("utf8 html");
    assert!(body.contains("blob-nums"));
    assert!(body.contains("<td class=\"blob-code\"><code>"));
    assert!(body.contains("1 &lt; 2"));
}

/// `GET /files/<path>` on a `.rs` blob renders syntax-highlighted source:
/// `arborium`'s `HtmlFormat::ClassNames` spans, matched by
/// `crate::assets::OVERRIDES`'s `.code .keyword`-family rules.
#[tokio::test]
async fn files_blob_view_syntax_highlights_a_rust_file() {
    let dir = seed_repo(&[("src/main.rs", "fn main() {\n    let ok = 1 < 2;\n}\n")]);
    let state = build_state_at(
        FixtureIdentity {
            name: "local-user",
            key: Keypair::from_seed(1),
        },
        dir.path().to_owned(),
    );
    let router = ents_web::router(state);

    let response = router
        .oneshot(
            Request::get("/files/src/main.rs")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body = String::from_utf8(body.to_vec()).expect("utf8 html");
    assert!(body.contains("blob-nums"));
    assert!(body.contains("class=\"code\""));
    assert!(body.contains("class=\"keyword\""));
}

/// The icon rail (`crate::pages::layout_shell`) names every top-level page
/// family truthfully: Dashboard, Code, Review, Tickets, Threads, then the
/// meta and account items -- and the issues family renders as its own rail
/// item, never behind the `META_SECTIONS` rail (see `crate::pages::mod`'s
/// own doc).
#[tokio::test]
async fn the_rail_carries_every_page_family_and_issues_left_the_meta_rail() {
    let state = build_state(FixtureIdentity {
        name: "local-user",
        key: Keypair::from_seed(1),
    });
    let router = ents_web::router(state);

    let overview = get_body(&router, "/").await;
    for href in [
        "/",
        "/files",
        "/commits",
        "/issues",
        "/comments",
        "/meta",
        "/account",
    ] {
        assert!(
            overview.contains(&format!("href=\"{href}\"")),
            "the rail links {href}"
        );
    }
    for label in ["Dashboard", "Code", "Review", "Tickets", "Threads"] {
        assert!(
            overview.contains(&format!("title=\"{label}\"")),
            "the rail tooltips {label}"
        );
    }

    let issues = get_body(&router, "/issues").await;
    assert!(
        !issues.contains("class=\"meta-rail\""),
        "issues renders as its own rail item, not behind the meta rail"
    );

    // The meta rail renders a bare (classless when inactive) link per
    // section; the icon rail's own issues link always carries a `title`
    // attribute, so this exact form only ever comes from the meta rail.
    let members = get_body(&router, "/members").await;
    assert!(
        !members.contains("<a href=\"/issues\">issues</a>"),
        "the meta rail no longer lists issues"
    );
}

/// The rail highlights exactly the item whose page family is being viewed
/// (`crate::pages::rail_link`'s `active` toggle): on `GET /files` the Code
/// item carries `class="active"` and the others do not.
#[tokio::test]
async fn the_rail_marks_the_active_item() {
    // A real on-disk repository: `GET /files` opens `state.path` itself.
    let dir = seed_repo(&[("README.md", "# hi\n")]);
    let state = build_state_at(
        FixtureIdentity {
            name: "local-user",
            key: Keypair::from_seed(1),
        },
        dir.path().to_owned(),
    );
    let router = ents_web::router(state);

    let files = get_body(&router, "/files").await;
    assert!(
        files.contains("class=\"active\" href=\"/files\""),
        "the Code item highlights on a files page"
    );
    assert!(
        files.contains("class=\"\" href=\"/commits\""),
        "the Review item stays unhighlighted there"
    );

    let comments = get_body(&router, "/comments").await;
    assert!(
        comments.contains("class=\"active\" href=\"/comments\""),
        "the Threads item highlights on the comments page"
    );
    assert!(
        comments.contains("class=\"\" href=\"/files\""),
        "the Code item stays unhighlighted there"
    );
}

/// The `meta` group (`crate::pages::mod`'s own doc): `GET /meta` is
/// reachable as the group's index page, and `GET /members` -- one of the
/// five page families that group shares -- renders with the
/// `META_SECTIONS` rail visible and the icon rail's meta item (not a
/// per-family item) highlighted.
#[tokio::test]
async fn meta_index_and_a_meta_group_page_render_with_the_rail() {
    let state = build_state(FixtureIdentity {
        name: "local-user",
        key: Keypair::from_seed(1),
    });
    let router = ents_web::router(state);

    let meta_response = router
        .clone()
        .oneshot(Request::get("/meta").body(Body::empty()).expect("request"))
        .await
        .expect("in-process call");
    assert_eq!(meta_response.status(), StatusCode::OK);

    let members_response = router
        .oneshot(
            Request::get("/members")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert_eq!(members_response.status(), StatusCode::OK);
    let body = members_response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body = String::from_utf8(body.to_vec()).expect("utf8 html");
    assert!(
        body.contains("class=\"meta-rail\""),
        "a meta-group page renders the section rail"
    );
    assert!(
        body.contains("class=\"active\" href=\"/meta\""),
        "the rail's meta item itself highlights, not a per-family item"
    );
}

/// `GET /files/<path>` renders a `.md` blob as Markdown and a `.adoc` blob
/// as AsciiDoc -- both a real rendered heading, not the raw source markup.
#[tokio::test]
async fn files_blob_view_renders_markdown_and_asciidoc_as_documents() {
    let dir = seed_repo(&[
        ("README.adoc", "= Welcome\n\nHello.\n"),
        ("docs/x.md", "# Doc Title\n\nSome text.\n"),
    ]);
    let state = build_state_at(
        FixtureIdentity {
            name: "local-user",
            key: Keypair::from_seed(1),
        },
        dir.path().to_owned(),
    );
    let router = ents_web::router(state);

    let adoc_response = router
        .clone()
        .oneshot(
            Request::get("/files/README.adoc")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert_eq!(adoc_response.status(), StatusCode::OK);
    let adoc_body = adoc_response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let adoc_body = String::from_utf8(adoc_body.to_vec()).expect("utf8 html");
    assert!(adoc_body.contains("<h1>Welcome</h1>"));
    assert!(!adoc_body.contains("= Welcome"));

    let md_response = router
        .oneshot(
            Request::get("/files/docs/x.md")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert_eq!(md_response.status(), StatusCode::OK);
    let md_body = md_response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let md_body = String::from_utf8(md_body.to_vec()).expect("utf8 html");
    assert!(md_body.contains("<h1>Doc Title</h1>"));
}

/// `GET /files/<path>` on a blob with no comments carries no comment-card
/// markup at all -- not even an empty section (`crate::pages::comments::comments_section`'s
/// own no-drop-but-no-empty-section contract).
#[tokio::test]
async fn files_blob_view_with_no_comments_has_no_comment_card_markup() {
    let dir = seed_repo(&[("src/main.rs", "line 1\nline 2\nline 3\n")]);
    let state = build_state_at(
        FixtureIdentity {
            name: "local-user",
            key: Keypair::from_seed(1),
        },
        dir.path().to_owned(),
    );
    let router = ents_web::router(state);

    let response = router
        .oneshot(
            Request::get("/files/src/main.rs")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body = String::from_utf8(body.to_vec()).expect("utf8 html");
    assert!(!body.contains("file-comments"));
    assert!(!body.contains("comment-meta"));
}

/// A blob view shows every comment anchored to it: author, body (rendered
/// as AsciiDoc), and its projected line range linking into the blob's own
/// `#L<n>` gutter.
#[tokio::test]
async fn files_blob_view_shows_a_seeded_comments_body_and_author() {
    let dir = seed_repo(&[("src/main.rs", "line 1\nline 2\nline 3\n")]);
    let state = build_state_at(
        FixtureIdentity {
            name: "commenter",
            key: Keypair::from_seed(1),
        },
        dir.path().to_owned(),
    );
    let router = ents_web::router(state.clone());
    seed_comment(
        &router,
        &state,
        "src/main.rs",
        "worth a look here",
        "2:2",
        "HEAD",
    )
    .await;

    let response = router
        .oneshot(
            Request::get("/files/src/main.rs")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body = String::from_utf8(body.to_vec()).expect("utf8 html");
    assert!(body.contains("id=\"comment-0\""));
    assert!(body.contains("worth a look here"));
    assert!(body.contains("commenter"));
    assert!(body.contains("href=\"#L2\""));
    assert!(!body.contains("class=\"outdated\""));
    // Interleaved directly into the blob, after line 2's row and before
    // line 3's -- not below the whole table.
    let line2 = body.find("id=\"L2\"").expect("line 2 renders");
    let card = body.find("comment-meta").expect("card renders");
    let line3 = body.find("id=\"L3\"").expect("line 3 renders");
    assert!(
        line2 < card && card < line3,
        "the card must land between line 2 and line 3, in document order"
    );
}

/// A comment whose anchored lines were since edited still renders (never
/// dropped), flagged with the muted `outdated` marker instead of a line
/// link (`ents_anchor::Projection::Outdated`).
#[tokio::test]
async fn files_blob_view_marks_an_outdated_comment() {
    let dir = seed_repo(&[("src/main.rs", "line 1\nline 2\nline 3\n")]);
    let state = build_state_at(
        FixtureIdentity {
            name: "commenter",
            key: Keypair::from_seed(1),
        },
        dir.path().to_owned(),
    );
    let router = ents_web::router(state.clone());
    seed_comment(
        &router,
        &state,
        "src/main.rs",
        "line two looks off",
        "2:2",
        "HEAD",
    )
    .await;
    // Edit exactly the anchored line, so the projection can no longer map
    // it -- `ents_anchor::project`'s own `Outdated` case.
    commit_change(
        dir.path(),
        "src/main.rs",
        "line 1\nsomething else entirely\nline 3\n",
        "edit line two",
    );

    let response = router
        .oneshot(
            Request::get("/files/src/main.rs")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body = String::from_utf8(body.to_vec()).expect("utf8 html");
    assert!(
        body.contains("line two looks off"),
        "comment is never dropped"
    );
    assert!(body.contains("class=\"outdated\""));
}

/// `model.comment-state`, `roots.web-session`: a comment resolves and
/// reopens through CSRF-checked, signed `POST`s -- each an
/// `ents_forge::comment::{resolve,reopen}` call -- and its `GET
/// /comments/{id}` page reflects the new state and offers the opposite
/// action each time. The wrong CSRF token is refused, exactly as every
/// other state-changing route in this crate refuses one.
#[tokio::test]
// @relation(model.comment-state, roots.web-signing, roots.web-session, scope=function, role=Verifies)
async fn a_comment_resolves_and_reopens_through_csrf_checked_posts() {
    let dir = seed_repo(&[("src/main.rs", "line 1\nline 2\nline 3\n")]);
    let state = build_state_at(
        FixtureIdentity {
            name: "commenter",
            key: Keypair::from_seed(1),
        },
        dir.path().to_owned(),
    );
    let router = ents_web::router(state.clone());
    let id = seed_comment(&router, &state, "src/main.rs", "look here", "2:2", "HEAD").await;
    let page = format!("/comments/{id}");
    let (cookie, csrf) = session_cookie_and_csrf(&router, &state, &page).await;

    // A fresh comment lists open and offers "resolve".
    let body = get_body(&router, &page).await;
    assert!(body.contains("resolve"), "an open comment offers resolve");

    // The wrong token is refused.
    let refused = router
        .clone()
        .oneshot(
            Request::post(format!("/comments/{id}/resolve"))
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::COOKIE, cookie.clone())
                .body(Body::from("csrf=not-the-token"))
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);

    // The right token resolves it.
    let resolved = router
        .clone()
        .oneshot(
            Request::post(format!("/comments/{id}/resolve"))
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::COOKIE, cookie.clone())
                .body(Body::from(format!("csrf={csrf}")))
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert!(resolved.status().is_redirection());
    let body = get_body(&router, &page).await;
    assert!(body.contains("resolved"), "the comment now reads resolved");
    assert!(body.contains("reopen"), "a resolved comment offers reopen");

    // Reopen returns it to open.
    let reopened = router
        .clone()
        .oneshot(
            Request::post(format!("/comments/{id}/reopen"))
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::COOKIE, cookie)
                .body(Body::from(format!("csrf={csrf}")))
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert!(reopened.status().is_redirection());
    let body = get_body(&router, &page).await;
    assert!(
        body.contains(">open<") || body.contains("resolve"),
        "the comment offers resolve again once reopened"
    );
}

/// `model.comment-thread`: a reply through `POST /comments/{id}/reply` is a
/// second comment (`ents_forge::comment::reply`) -- after it lands, the
/// comment index lists two comments where the seed left one.
#[tokio::test]
// @relation(model.comment-thread, roots.web-signing, roots.web-session, scope=function, role=Verifies)
async fn a_reply_creates_a_threaded_comment_through_a_signed_post() {
    let dir = seed_repo(&[("src/main.rs", "line 1\nline 2\nline 3\n")]);
    let state = build_state_at(
        FixtureIdentity {
            name: "commenter",
            key: Keypair::from_seed(1),
        },
        dir.path().to_owned(),
    );
    let router = ents_web::router(state.clone());
    let id = seed_comment(&router, &state, "src/main.rs", "the parent", "2:2", "HEAD").await;
    let (cookie, csrf) = session_cookie_and_csrf(&router, &state, "/comments").await;

    let reply = router
        .clone()
        .oneshot(
            Request::post(format!("/comments/{id}/reply"))
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::COOKIE, cookie)
                .body(Body::from(format!("body=a+reply+here&csrf={csrf}")))
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert!(reply.status().is_redirection(), "{:?}", reply.status());

    let body = get_body(&router, "/comments").await;
    let count = body.matches("/comments/").count();
    assert!(
        count >= 2,
        "the index lists the parent and its reply, got {count} links"
    );
    assert!(body.contains("a reply here"), "the reply's body renders");
}

/// A raw-source blob view carries the client-side hooks `assets/ents.js`
/// needs: `div.blob`'s own `data-path`/`data-rev` (the latter a full
/// 40-hex `HEAD` commit oid, not the string `"HEAD"`), and a
/// `<template id="composer-template">` whose form carries a csrf input and
/// hidden `path`/`rev` inputs pre-filled with this exact file and commit.
#[tokio::test]
async fn files_blob_view_carries_data_path_data_rev_and_the_composer_template() {
    let dir = seed_repo(&[("src/main.rs", "fn main() {}\n")]);
    let oid = head_oid(dir.path());
    assert_eq!(oid.len(), 40, "a full sha1 hex oid is 40 characters");
    let state = build_state_at(
        FixtureIdentity {
            name: "local-user",
            key: Keypair::from_seed(1),
        },
        dir.path().to_owned(),
    );
    let router = ents_web::router(state);

    let response = router
        .oneshot(
            Request::get("/files/src/main.rs")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body = String::from_utf8(body.to_vec()).expect("utf8 html");
    assert!(body.contains("data-path=\"src/main.rs\""));
    assert!(body.contains(&format!("data-rev=\"{oid}\"")));
    let template_start = body
        .find("id=\"composer-template\"")
        .expect("composer template renders");
    let template = body.get(template_start..).expect("template slice");
    assert!(
        template.contains("name=\"csrf\""),
        "the composer's own form carries a csrf input"
    );
    assert!(template.contains(r#"name="path" value="src/main.rs""#));
    assert!(template.contains(&format!(r#"name="rev" value="{oid}""#)));
}

/// `GET /ents.js` serves the client-side script `crate::pages::layout`
/// loads with `defer` -- no session required (mirrors `GET /style.css`'s
/// own stance), a JS content type, and a non-empty body.
#[tokio::test]
async fn ents_js_is_served_with_a_javascript_content_type() {
    let state = build_state(FixtureIdentity {
        name: "local-user",
        key: Keypair::from_seed(1),
    });
    let router = ents_web::router(state);

    let response = router
        .oneshot(
            Request::get("/ents.js")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .expect("content-type")
        .to_str()
        .expect("ascii")
        .to_owned();
    assert!(content_type.contains("javascript"));
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    assert!(!body.is_empty());
}

/// The blob header bar (`crate::pages::files::blob_header`) shows a raw
/// source file's line count, human-formatted size, and detected language,
/// plus the "comment on this file" no-JS fallback link -- moved here from
/// `crumbs`'s own trailing edge.
#[tokio::test]
async fn files_blob_header_shows_line_count_size_language_and_the_comment_link() {
    let dir = seed_repo(&[("src/main.rs", "fn main() {\n    let ok = 1;\n}\n")]);
    let state = build_state_at(
        FixtureIdentity {
            name: "local-user",
            key: Keypair::from_seed(1),
        },
        dir.path().to_owned(),
    );
    let router = ents_web::router(state);

    let response = router
        .oneshot(
            Request::get("/files/src/main.rs")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body = String::from_utf8(body.to_vec()).expect("utf8 html");
    assert!(body.contains("blob-header"));
    assert!(body.contains("3 lines"));
    assert!(body.contains("rust"));
    assert!(body.contains("comment on this file"));
}

/// `GET /files` (a directory listing): a file entry carries a
/// human-formatted size (`span.entry-size`), rendered after the
/// directory-first entries, which carry none.
#[tokio::test]
async fn files_root_listing_shows_a_size_for_a_file_but_not_a_directory() {
    let dir = seed_repo(&[("README.md", "# hi\n"), ("src/main.rs", "fn main() {}\n")]);
    let state = build_state_at(
        FixtureIdentity {
            name: "local-user",
            key: Keypair::from_seed(1),
        },
        dir.path().to_owned(),
    );
    let router = ents_web::router(state);

    let response = router
        .oneshot(Request::get("/files").body(Body::empty()).expect("request"))
        .await
        .expect("in-process call");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body = String::from_utf8(body.to_vec()).expect("utf8 html");
    let src_index = body
        .find("/files/src\"")
        .expect("the src directory links in");
    let size_index = body.find("entry-size").expect("a size span renders");
    assert!(
        src_index < size_index,
        "the directory row (sorted first, no size cell) renders before the file row's own size"
    );
}

/// A doc-rendered (Markdown) blob view carries no composer template at
/// all: there is no source line row for `assets/ents.js` to anchor an
/// inline composer after, so `ents_web::pages::files::blob_view` never
/// renders one for this view kind.
#[tokio::test]
async fn files_markdown_blob_view_has_no_composer_template() {
    let dir = seed_repo(&[("docs/x.md", "# Doc Title\n\nSome text.\n")]);
    let state = build_state_at(
        FixtureIdentity {
            name: "local-user",
            key: Keypair::from_seed(1),
        },
        dir.path().to_owned(),
    );
    let router = ents_web::router(state);

    let response = router
        .oneshot(
            Request::get("/files/docs/x.md")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body = String::from_utf8(body.to_vec()).expect("utf8 html");
    assert!(
        !body.contains("composer-template"),
        "a doc-rendered view has no source line to anchor a composer to"
    );
}

/// `GET /comments?file=<path>&lines=<range>` pre-fills the add-comment
/// form's `path`/`lines` fields -- the entry point `crate::pages::files`'s
/// own "comment on this file" link uses.
#[tokio::test]
async fn comments_list_prefills_the_add_form_from_query_params() {
    let state = build_state(FixtureIdentity {
        name: "local-user",
        key: Keypair::from_seed(1),
    });
    let router = ents_web::router(state);

    let response = router
        .oneshot(
            Request::get("/comments?file=src/main.rs&lines=1-2")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body = String::from_utf8(body.to_vec()).expect("utf8 html");
    assert!(body.contains(r#"name="path" value="src/main.rs""#));
    assert!(body.contains(r#"name="lines" value="1-2""#));
    assert!(
        body.contains(r#"name="rev" value="HEAD""#),
        "rev defaults to HEAD when absent, exactly as before"
    );
}

/// `GET /comments?rev=<oid>` (the link `crate::pages::commits::show`'s
/// "comment on this commit" renders) carries the given rev through into
/// the add form, rather than defaulting to `HEAD`.
#[tokio::test]
async fn comments_list_prefills_rev_from_the_query_param() {
    let state = build_state(FixtureIdentity {
        name: "local-user",
        key: Keypair::from_seed(1),
    });
    let router = ents_web::router(state);

    let response = router
        .oneshot(
            Request::get("/comments?rev=deadbeef")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body = String::from_utf8(body.to_vec()).expect("utf8 html");
    assert!(body.contains(r#"name="rev" value="deadbeef""#));
}

/// `GET /commits` lists the repository's commit history: the seeded
/// commit's own short id appears, linking into `/commit/{oid}`.
#[tokio::test]
async fn commits_list_shows_a_fixture_commit() {
    let dir = seed_repo(&[("README.md", "# hi\n")]);
    let oid = head_oid(dir.path());
    let state = build_state_at(
        FixtureIdentity {
            name: "local-user",
            key: Keypair::from_seed(1),
        },
        dir.path().to_owned(),
    );
    let router = ents_web::router(state);

    let response = router
        .oneshot(
            Request::get("/commits")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body = String::from_utf8(body.to_vec()).expect("utf8 html");
    assert!(body.contains(&format!("/commit/{oid}")));
    assert!(body.contains("seed"), "the seeded commit's subject renders");
}

/// `GET /commit/{oid}` shows the commit's subject, its author, and a
/// colorized diff line for the file it introduced.
#[tokio::test]
async fn commit_show_renders_the_subject_and_a_diff_line() {
    let dir = seed_repo(&[("README.md", "# hi\n")]);
    let oid = head_oid(dir.path());
    let state = build_state_at(
        FixtureIdentity {
            name: "local-user",
            key: Keypair::from_seed(1),
        },
        dir.path().to_owned(),
    );
    let router = ents_web::router(state);

    let response = router
        .oneshot(
            Request::get(format!("/commit/{oid}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body = String::from_utf8(body.to_vec()).expect("utf8 html");
    assert!(body.contains("seed"), "the commit's subject renders");
    assert!(
        body.contains("class=\"ln add\""),
        "the root commit's diff renders its added lines"
    );
}

/// `GET /commit/{oid}` renders one `.file` header per changed blob and
/// none for the intermediate directories the tree walk also names --
/// each subdirectory used to appear as its own bare file section.
#[tokio::test]
async fn commit_diff_lists_files_not_intermediate_directories() {
    let dir = seed_repo(&[("crates/foo/src/lib.rs", "pub fn f() {}\n")]);
    let oid = head_oid(dir.path());
    let state = build_state_at(
        FixtureIdentity {
            name: "local-user",
            key: Keypair::from_seed(1),
        },
        dir.path().to_owned(),
    );
    let router = ents_web::router(state);

    let response = router
        .oneshot(
            Request::get(format!("/commit/{oid}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body = String::from_utf8(body.to_vec()).expect("utf8 html");
    assert_eq!(
        body.matches("class=\"ln file\"").count(),
        1,
        "one changed blob means exactly one file header"
    );
}

/// `GET /commit/{oid}` lists, under a "conversation" heading, every
/// comment whose anchor was captured against that exact commit -- and
/// none captured against a different one, even a later commit on the same
/// branch. The "comment on this commit" link prefills `rev` to the shown
/// commit's own oid.
#[tokio::test]
async fn commit_show_lists_comments_captured_against_that_exact_commit() {
    let dir = seed_repo(&[("src/main.rs", "line 1\nline 2\nline 3\n")]);
    let first_oid = head_oid(dir.path());
    let state = build_state_at(
        FixtureIdentity {
            name: "commenter",
            key: Keypair::from_seed(1),
        },
        dir.path().to_owned(),
    );
    let router = ents_web::router(state.clone());
    seed_comment(
        &router,
        &state,
        "src/main.rs",
        "left at the first commit",
        "2:2",
        &first_oid,
    )
    .await;

    commit_change(
        dir.path(),
        "src/main.rs",
        "line 1\nline two\nline 3\n",
        "second commit",
    );
    let second_oid = head_oid(dir.path());
    assert_ne!(first_oid, second_oid);

    let first_response = router
        .clone()
        .oneshot(
            Request::get(format!("/commit/{first_oid}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert_eq!(first_response.status(), StatusCode::OK);
    let first_body = String::from_utf8(
        first_response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes()
            .to_vec(),
    )
    .expect("utf8 html");
    assert!(first_body.contains("Conversation"));
    assert!(first_body.contains("left at the first commit"));
    assert!(first_body.contains("commenter"));
    assert!(
        first_body.contains("href=\"/files/src/main.rs#L2\""),
        "the conversation card links path#lines into the file browser: {first_body}"
    );
    assert!(
        first_body.contains(&format!("href=\"/comments?rev={first_oid}\"")),
        "the comment-on-this-commit link prefills this commit's own oid: {first_body}"
    );

    let second_response = router
        .oneshot(
            Request::get(format!("/commit/{second_oid}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert_eq!(second_response.status(), StatusCode::OK);
    let second_body = String::from_utf8(
        second_response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes()
            .to_vec(),
    )
    .expect("utf8 html");
    assert!(
        !second_body.contains("left at the first commit"),
        "a comment captured against the first commit must not appear on the second: {second_body}"
    );
}

/// `model.review`, `model.comment-context`: starting a review on a commit
/// page (`POST /commit/{oid}/review`, `ents_forge::review::new`) makes its
/// verdict, body, and reviewer render on that commit's page, and a comment
/// on the review (`POST /reviews/{id}/comment`) joins the review's own
/// discussion thread -- every step a CSRF-checked signed POST through the
/// injected identity.
#[tokio::test]
// @relation(model.review, model.review-pin, model.comment-context, roots.web-signing, roots.web-session, scope=function, role=Verifies)
async fn commit_page_shows_a_seeded_review_verdict_and_a_review_comment() {
    let dir = seed_repo(&[("src/main.rs", "fn main() {}\n")]);
    let oid = head_oid(dir.path());
    let state = build_state_at(
        FixtureIdentity {
            name: "reviewer",
            key: Keypair::from_seed(1),
        },
        dir.path().to_owned(),
    );
    let router = ents_web::router(state.clone());

    // Start a review of this commit through the commit page's own form.
    let (cookie, csrf) = session_cookie_and_csrf(&router, &state, &format!("/commit/{oid}")).await;
    let started = router
        .clone()
        .oneshot(
            Request::post(format!("/commit/{oid}/review"))
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::COOKIE, cookie.clone())
                .body(Body::from(format!(
                    "verdict=request-changes&body=needs+a+test&csrf={csrf}"
                )))
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert!(started.status().is_redirection(), "{:?}", started.status());

    // The commit page renders the review's verdict, body, and reviewer.
    let page = get_body(&router, &format!("/commit/{oid}")).await;
    assert!(page.contains("Reviews"), "the reviews section renders");
    assert!(
        page.contains("class=\"verdict\""),
        "the verdict renders prominently"
    );
    assert!(page.contains("request-changes"), "the verdict text renders");
    assert!(page.contains("needs a test"), "the review body renders");
    assert!(
        page.contains("reviewer"),
        "the reviewer's own identity (from the commit chain) renders"
    );

    // Recover the review id from its comment form's action, then comment on
    // the review; the comment joins the review's thread on the same page.
    let review_id = page
        .split_once("/reviews/")
        .and_then(|(_, rest)| rest.split_once("/comment"))
        .map(|(id, _)| id)
        .expect("a review comment form links in");

    let commented = router
        .clone()
        .oneshot(
            Request::post(format!("/reviews/{review_id}/comment"))
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::COOKIE, cookie)
                .body(Body::from(format!(
                    "body=agreed+on+the+test&return_to=/commit/{oid}&csrf={csrf}"
                )))
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert!(
        commented.status().is_redirection(),
        "{:?}",
        commented.status()
    );
    let page = get_body(&router, &format!("/commit/{oid}")).await;
    assert!(
        page.contains("agreed on the test"),
        "the review comment renders in the review's thread: {page}"
    );
}

/// `GET /commit/{oid}` on a malformed id is a 404, never a panic or 500.
#[tokio::test]
async fn commit_show_on_an_invalid_oid_is_not_found_not_a_crash() {
    let state = build_state(FixtureIdentity {
        name: "local-user",
        key: Keypair::from_seed(1),
    });
    let router = ents_web::router(state);

    let response = router
        .oneshot(
            Request::get("/commit/zzz")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// `POST /issues` through the real signed-write path
/// (`ents_forge::issue::new`), returning the new issue's id from the
/// redirect's `Location` (`/issues/<id>`). Asserts the write succeeded.
async fn seed_issue(
    router: &axum::Router,
    state: &AppState<ObjectStore>,
    title: &str,
    issue_state: &str,
    assignees: &str,
    labels: &str,
) -> String {
    let (cookie, csrf) = session_cookie_and_csrf(router, state, "/issues").await;
    let form = format!(
        "title={}&state={issue_state}&assignees={assignees}&labels={labels}&body=the+full+body&csrf={csrf}",
        title.replace(' ', "+")
    );
    let response = router
        .clone()
        .oneshot(
            Request::post("/issues")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::COOKIE, cookie)
                .body(Body::from(form))
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert!(
        response.status().is_redirection(),
        "issue create did not succeed: {:?}",
        response.status()
    );
    response
        .headers()
        .get(header::LOCATION)
        .expect("a successful issue create redirects to the new issue")
        .to_str()
        .expect("ascii")
        .strip_prefix("/issues/")
        .expect("redirect targets /issues/<id>")
        .to_owned()
}

/// `model.issue`, `model.comment-context`: the issues index lists a seeded
/// issue with its state, assignees, and labels; the detail page shows the
/// issue and its discussion thread; a comment naming `issues/<id>` as its
/// context joins that thread; and an edit changes the issue's state -- every
/// mutation a CSRF-checked signed POST calling the same `ents_forge` funcs
/// the CLI and lens do.
#[tokio::test]
// @relation(model.issue, model.comment-context, roots.web-signing, roots.web-session, scope=function, role=Verifies)
async fn issues_index_and_detail_render_a_seeded_issue_and_its_context_comment() {
    let state = build_state(FixtureIdentity {
        name: "filer",
        key: Keypair::from_seed(1),
    });
    let router = ents_web::router(state.clone());
    let id = seed_issue(
        &router,
        &state,
        "gate rejects a valid signature",
        "triaged",
        "jdc",
        "bug",
    )
    .await;

    // The index lists the issue with its state, assignees, and labels, and
    // links into its detail page.
    let index = get_body(&router, "/issues").await;
    assert!(index.contains("gate rejects a valid signature"));
    assert!(index.contains("triaged"));
    assert!(index.contains("jdc"));
    assert!(index.contains("bug"));
    assert!(index.contains(&format!("/issues/{id}")));

    // The detail page shows the issue and an (initially empty) discussion.
    let detail = get_body(&router, &format!("/issues/{id}")).await;
    assert!(detail.contains("gate rejects a valid signature"));
    assert!(detail.contains("the full body"));
    assert!(detail.contains("Discussion"));

    // A comment naming the issue as its context joins the thread.
    let (cookie, csrf) = session_cookie_and_csrf(&router, &state, "/issues").await;
    let comment = router
        .clone()
        .oneshot(
            Request::post(format!("/issues/{id}/comment"))
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::COOKIE, cookie.clone())
                .body(Body::from(format!("body=cannot+reproduce+yet&csrf={csrf}")))
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert!(comment.status().is_redirection(), "{:?}", comment.status());
    let detail = get_body(&router, &format!("/issues/{id}")).await;
    assert!(
        detail.contains("cannot reproduce yet"),
        "the context comment renders in the issue's thread: {detail}"
    );

    // Editing the issue's state lands and reads back.
    let edited = router
        .clone()
        .oneshot(
            Request::post(format!("/issues/{id}"))
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::COOKIE, cookie)
                .body(Body::from(format!("state=closed&csrf={csrf}")))
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert!(edited.status().is_redirection());
    let detail = get_body(&router, &format!("/issues/{id}")).await;
    assert!(detail.contains("closed"), "the edited state reads back");
}

/// `roots.web-session`: opening an issue is a state-changing route, so a
/// `POST /issues` with no CSRF field at all is rejected, and one with the
/// wrong token is a bad request -- the same gate every mutation in this
/// crate runs behind.
#[tokio::test]
// @relation(model.issue, roots.web-session, scope=function, role=Verifies)
async fn issue_create_is_rejected_without_a_valid_csrf_token() {
    let state = build_state(FixtureIdentity {
        name: "filer",
        key: Keypair::from_seed(1),
    });
    let router = ents_web::router(Arc::clone(&state));

    // No CSRF field at all.
    let no_csrf = router
        .clone()
        .oneshot(
            Request::post("/issues")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("title=sneaky&state=open"))
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert!(
        !no_csrf.status().is_success() && !no_csrf.status().is_redirection(),
        "a POST with no csrf field must not open an issue"
    );

    // A session's cookie, but the wrong token.
    let (cookie, _csrf) = session_cookie_and_csrf(&router, &state, "/issues").await;
    let wrong = router
        .oneshot(
            Request::post("/issues")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::COOKIE, cookie)
                .body(Body::from("title=sneaky&state=open&csrf=not-the-token"))
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert_eq!(wrong.status(), StatusCode::BAD_REQUEST);
}

/// `GET /search?q=` finds a known fixture file path, linking into its
/// `/files/...` blob view.
#[tokio::test]
async fn search_finds_a_known_fixture_file_path() {
    let dir = seed_repo(&[("src/needle.rs", "fn main() {}\n")]);
    let state = build_state_at(
        FixtureIdentity {
            name: "local-user",
            key: Keypair::from_seed(1),
        },
        dir.path().to_owned(),
    );
    let router = ents_web::router(state);

    let response = router
        .oneshot(
            Request::get("/search?q=needle")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body = String::from_utf8(body.to_vec()).expect("utf8 html");
    assert!(body.contains("/files/src/needle.rs"));
}

/// `GET /search` with no query renders a "type to search" blankslate
/// naming the header's own search input -- not a "no matches" one, since
/// nothing was searched yet -- rather than an empty or error page.
#[tokio::test]
async fn search_with_no_query_renders_a_blankslate() {
    let state = build_state(FixtureIdentity {
        name: "local-user",
        key: Keypair::from_seed(1),
    });
    let router = ents_web::router(state);

    let response = router
        .oneshot(
            Request::get("/search")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body = String::from_utf8(body.to_vec()).expect("utf8 html");
    assert!(body.contains("Type to search"));
    assert!(
        body.contains("Jump to file or symbol"),
        "the prompt names the header's own search input"
    );
}

/// `GET /comments` surfaces a comment ref written by an older schema
/// through the shared unreadable disclosure instead of silently dropping
/// it, and its own `GET /comments/{id}` page renders the plain unreadable
/// marker card rather than erroring.
#[tokio::test]
async fn comments_surface_an_unreadable_ref_in_the_list_and_on_its_own_page() {
    let refs = MemRefStore::default();
    let objects = ObjectStore::default();
    let tip = write_commit(
        &objects,
        &CommitSpec {
            tree: ents_testutil::empty_tree(&objects),
            parents: Vec::new(),
            message: "legacy comment".to_owned(),
            seconds: 100,
        },
        None,
    );
    let refname: gix::refs::FullName = "refs/meta/comments/legacy"
        .try_into()
        .expect("valid refname");
    refs.set(refname.as_ref(), tip);

    let state = build_state_with(
        FixtureIdentity {
            name: "local-user",
            key: Keypair::from_seed(1),
        },
        refs,
        objects,
    );
    let router = ents_web::router(state);

    let list = get_body(&router, "/comments").await;
    assert!(
        list.contains("unreadable-note") && list.contains("1 unreadable"),
        "the list page carries the subtle disclosure: {list}"
    );
    assert!(
        list.contains("refs/meta/comments/legacy"),
        "the disclosure names the failed ref"
    );

    let detail = get_body(&router, "/comments/legacy").await;
    assert!(
        detail.contains("unreadable"),
        "the detail page shows the error state plainly instead of erroring: {detail}"
    );
}

/// `POST /effects` defines an effect as a signed mutation on
/// `refs/meta/effects/<name>` and redirects to its show page; the list
/// page then names it -- the web counterpart of `git ents effect add`.
#[tokio::test]
async fn effect_form_defines_an_effect() {
    let dir = seed_repo(&[("README.md", "# hi\n")]);
    let state = build_state_at(
        FixtureIdentity {
            name: "local-user",
            key: Keypair::from_seed(1),
        },
        dir.path().to_owned(),
    );
    let router = ents_web::router(state.clone());
    let (cookie, csrf) = session_cookie_and_csrf(&router, &state, "/effects").await;

    let form = format!(
        "name=unit&trigger=rev(refs/heads/main)&run=cargo+nextest+run&toolchains=rust&csrf={csrf}"
    );
    let response = router
        .clone()
        .oneshot(
            Request::post("/effects")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::COOKIE, cookie)
                .body(Body::from(form))
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert!(
        response.status().is_redirection(),
        "effect write did not succeed: {:?}",
        response.status()
    );

    let list = get_body(&router, "/effects").await;
    assert!(list.contains("unit"), "the new effect lists: {list}");
    let show = get_body(&router, "/effects/unit").await;
    assert!(
        show.contains("rev(refs/heads/main)") && show.contains("parses"),
        "the show page renders the trigger and its parse check: {show}"
    );
}

/// `POST /toolchains` records a toolchain from a recipe given as text
/// (`ents_kiln::toolchain::register`) and redirects to its show page --
/// the recipe-flow counterpart of `git ents toolchain import`.
#[tokio::test]
async fn toolchain_form_registers_a_recipe() {
    let dir = seed_repo(&[("README.md", "# hi\n")]);
    let state = build_state_at(
        FixtureIdentity {
            name: "local-user",
            key: Keypair::from_seed(1),
        },
        dir.path().to_owned(),
    );
    let router = ents_web::router(state.clone());
    let (cookie, csrf) = session_cookie_and_csrf(&router, &state, "/toolchains").await;

    let form =
        format!("name=empty&recipe=embedded+4b825dc642cb6eb9a060e54bf8d69288fbee4904&csrf={csrf}");
    let response = router
        .clone()
        .oneshot(
            Request::post("/toolchains")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::COOKIE, cookie)
                .body(Body::from(form))
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert!(
        response.status().is_redirection(),
        "toolchain write did not succeed: {:?}",
        response.status()
    );

    let list = get_body(&router, "/toolchains").await;
    assert!(list.contains("empty"), "the new toolchain lists: {list}");
    let show = get_body(&router, "/toolchains/empty").await;
    assert!(
        show.contains("Embedded"),
        "the show page renders the recorded recipe: {show}"
    );
}

/// The issues page carries a `datalist#members` of enrolled usernames so
/// the assignees field completes by member id in place.
#[tokio::test]
async fn issue_forms_carry_a_members_datalist() {
    let dir = seed_repo(&[("README.md", "# hi\n")]);
    let state = build_state_at(
        FixtureIdentity {
            name: "local-user",
            key: Keypair::from_seed(1),
        },
        dir.path().to_owned(),
    );
    let router = ents_web::router(state);
    let body = get_body(&router, "/issues").await;
    assert!(
        body.contains("datalist id=\"members\""),
        "the assignees field has a members datalist to complete from: {body}"
    );
}

/// A show page for an id with no ref at all is a real 404, not a 500 --
/// `ents_forge::Error::NotFound` keeps its status through the `Forge`
/// box (the box exists for variant-size hygiene only).
#[tokio::test]
async fn missing_forge_entity_is_a_404_not_a_500() {
    let dir = seed_repo(&[("README.md", "# hi\n")]);
    let state = build_state_at(
        FixtureIdentity {
            name: "local-user",
            key: Keypair::from_seed(1),
        },
        dir.path().to_owned(),
    );
    let router = ents_web::router(state);
    for path in ["/issues/nope", "/comments/nope"] {
        let response = router
            .clone()
            .oneshot(Request::get(path).body(Body::empty()).expect("request"))
            .await
            .expect("in-process call");
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "GET {path}");
    }
}

/// `GET /toolchains` surfaces a toolchain written by an older schema
/// (piece 1's bug: this repository's own
/// `refs/meta/toolchains/{rust,sccache,zig}` still carry it) through the
/// shared unreadable disclosure, never a 500 -- and a good toolchain
/// alongside it still lists and links normally.
#[tokio::test]
async fn toolchains_list_marks_a_legacy_entry_but_still_lists_a_good_one() {
    let refs = MemRefStore::default();
    let objects = ObjectStore::default();
    let name: gix::refs::FullName = "refs/meta/toolchains/good".try_into().expect("valid");
    write_meta_entity(
        &refs,
        &objects,
        name,
        &Toolchain {
            name: "good".to_owned(),
            recipe: "embedded 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n".to_owned(),
        },
        None,
        100,
    );
    write_legacy_toolchain(&refs, &objects, "legacy");

    let state = build_state_with(
        FixtureIdentity {
            name: "local-user",
            key: Keypair::from_seed(1),
        },
        refs,
        objects,
    );
    let router = ents_web::router(state);

    let response = router
        .oneshot(
            Request::get("/toolchains")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body = String::from_utf8(body.to_vec()).expect("utf8 html");
    assert!(body.contains(r#"href="/toolchains/good""#));
    assert!(body.contains(r#"href="/toolchains/legacy""#));
    assert!(
        body.contains("unreadable-note") && body.contains("1 unreadable"),
        "the legacy entry surfaces through the shared disclosure: {body}"
    );
    assert!(
        body.contains("refs/meta/toolchains/legacy"),
        "the disclosure names the failed ref"
    );
}

/// `GET /toolchains/{name}` on a legacy-schema entry renders the marker
/// card (with the underlying error) rather than a 500.
#[tokio::test]
async fn toolchain_show_on_a_legacy_entry_renders_a_marker_not_a_500() {
    let refs = MemRefStore::default();
    let objects = ObjectStore::default();
    write_legacy_toolchain(&refs, &objects, "legacy");

    let state = build_state_with(
        FixtureIdentity {
            name: "local-user",
            key: Keypair::from_seed(1),
        },
        refs,
        objects,
    );
    let router = ents_web::router(state);

    let response = router
        .oneshot(
            Request::get("/toolchains/legacy")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body = String::from_utf8(body.to_vec()).expect("utf8 html");
    assert!(body.contains("unreadable"));
    assert!(
        body.contains("is not a blob"),
        "the underlying facet-git-tree error renders verbatim: {body}"
    );
}

/// A real, readable entity (not merely an empty ref store) exercised on
/// every list/show page pair `read_all`'s `state.objects()` double-lock
/// regression could hit: each page must complete rather than hang forever
/// (a non-reentrant `Mutex` self-deadlock, previously reachable whenever a
/// row's tree actually read back cleanly -- see the fix commit's own
/// message). `#[tokio::test]`'s single-threaded runtime means a real
/// deadlock here hangs the whole test binary rather than merely failing
/// it, so this is worth pinning down explicitly rather than trusting the
/// list/show pages' other tests to happen to seed data.
#[tokio::test]
async fn members_effects_redactions_and_toolchains_list_and_show_a_real_entity_without_hanging() {
    let refs = MemRefStore::default();
    let objects = ObjectStore::default();
    enroll_member(
        &refs,
        &objects,
        "jdc",
        &Keypair::from_seed(1),
        Provenance::AdminRegistered,
        100,
    );
    let effect_name: gix::refs::FullName = "refs/meta/effects/ci".try_into().expect("valid");
    write_meta_entity(
        &refs,
        &objects,
        effect_name,
        &Effect {
            name: "ci".to_owned(),
            trigger: "rev(refs/heads/main)".to_owned(),
            toolchains: vec![],
            run: "true".to_owned(),
        },
        None,
        100,
    );
    let redaction_name: gix::refs::FullName = "refs/meta/redactions/1".try_into().expect("valid");
    write_meta_entity(
        &refs,
        &objects,
        redaction_name,
        &Redaction::new(gix_hash::ObjectId::null(gix_hash::Kind::Sha1), "leaked"),
        None,
        100,
    );
    let toolchain_name: gix::refs::FullName =
        "refs/meta/toolchains/good".try_into().expect("valid");
    write_meta_entity(
        &refs,
        &objects,
        toolchain_name,
        &Toolchain {
            name: "good".to_owned(),
            recipe: "embedded 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n".to_owned(),
        },
        None,
        100,
    );

    let state = build_state_with(
        FixtureIdentity {
            name: "local-user",
            key: Keypair::from_seed(2),
        },
        refs,
        objects,
    );
    let router = ents_web::router(state);

    for path in [
        "/members",
        "/members/jdc",
        "/effects",
        "/effects/ci",
        "/redactions",
        "/redactions/1",
        "/toolchains",
        "/toolchains/good",
    ] {
        let response = router
            .clone()
            .oneshot(Request::get(path).body(Body::empty()).expect("request"))
            .await
            .expect("in-process call");
        assert_eq!(response.status(), StatusCode::OK, "GET {path}");
    }
}
