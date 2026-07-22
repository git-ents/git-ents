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
use ents_model::{Account, Effect, MemberId, Provenance, Redaction, ResultRecord, Status};
use ents_receive::{Identity, Mode, NullEventSink};
use ents_testutil::{
    CommitSpec, Keypair, MemRefStore, ObjectStore, enroll_member, record_result, write_commit,
    write_meta_entity,
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
    assert!(body.contains("--bg"));
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
    assert!(body.contains("Working tree"));
    assert!(body.contains("Issues"));
    // The shell chrome renders on every page: the icon rail, the sticky
    // top bar, and the bar's palette search form.
    assert!(body.contains("class=\"rail\""));
    assert!(body.contains("class=\"wb-bar\""));
    assert!(body.contains("class=\"palette\""));
    assert!(body.contains("Jump to file, commit, issue, member"));
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

/// `roots.web-agnostic`: the workbench dashboard (`GET /`) renders its
/// four sections -- Working tree, Needs attention, Issues, History --
/// against a real repository, with real content in each: the dirty file
/// shows up as a working-tree row, the seeded open comment as a
/// needs-attention row (naming its anchored path), the seeded open issue
/// as an issue, and the `HEAD` commit in the History card with its
/// Scoped-Commits scope chip.
#[tokio::test]
async fn dashboard_renders_the_four_sections_with_real_content() {
    let dir = seed_repo(&[("src/main.rs", "line 1\nline 2\nline 3\n")]);
    // Commit a scoped subject so the History card has a chip to parse.
    commit_change(
        dir.path(),
        "src/main.rs",
        "line 1\nline 2\nline 3\nline 4\n",
        "model: grow main by a line",
    );
    let oid = head_oid(dir.path());
    // Dirty the working tree after the commit, for the Working tree lane.
    std::fs::write(dir.path().join("src/main.rs"), "changed\n").expect("dirty the tree");
    let state = build_state_at(
        FixtureIdentity {
            name: "local-user",
            key: Keypair::from_seed(1),
        },
        dir.path().to_owned(),
    );
    let router = ents_web::router(state.clone());
    let comment_id =
        seed_comment(&router, &state, "src/main.rs", "worth a look", "2:2", &oid).await;
    seed_issue(&router, &state, "Ship the desk", "open", "", "").await;

    let body = get_body(&router, "/").await;
    for header in ["Working tree", "Needs attention", "Issues", "History"] {
        assert!(body.contains(header), "the {header} section renders");
    }
    assert!(
        body.contains("href=\"/files/src/main.rs\"") && body.contains("modified"),
        "the dirty file lists as a working-tree change"
    );
    assert!(
        body.contains(&format!("/comments/{comment_id}")) && body.contains("src/main.rs:2"),
        "the open comment links out and names its anchored path"
    );
    assert!(
        body.contains("Ship the desk"),
        "the open issue lists on the Issues card"
    );
    assert!(
        body.contains(&format!("/commit/{oid}")),
        "the History card links the HEAD commit"
    );
    assert!(
        body.contains("class=\"scope scope-c") && body.contains(">model</span>"),
        "the scoped subject chips its scope"
    );
}

/// `GET /` on an unborn `HEAD` (a freshly initialized, still-empty
/// repository) still renders all four sections, each degrading to its own
/// empty-state row rather than a placeholder commit or a 500.
#[tokio::test]
async fn dashboard_degrades_every_section_on_an_unborn_head() {
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

    let body = get_body(&router, "/").await;
    for header in ["Working tree", "Needs attention", "Issues", "History"] {
        assert!(body.contains(header), "the {header} section renders");
    }
    assert!(!body.contains("/commit/"), "no placeholder commit links");
    assert!(body.contains("No commits yet."));
}

/// `GET /account` states who the session is (`roots.web-signing`): with
/// the serving identity's key enrolled, the page renders that member's
/// own identity card (never a login or signup form -- the signing key is
/// the identity); with no matching member, it shows the unenrolled key
/// itself.
#[tokio::test]
// @relation(roots.web-signing, scope=function, role=Verifies)
async fn account_page_names_the_signed_in_member() {
    let key = Keypair::from_seed(1);
    let refs = MemRefStore::default();
    let objects = ObjectStore::default();
    enroll_member(
        &refs,
        &objects,
        "joey",
        &key,
        Provenance::AdminRegistered,
        100,
    );
    let state = build_state_with(
        FixtureIdentity {
            name: "local-user",
            key: Keypair::from_seed(1),
        },
        refs,
        objects,
    );
    let body = get_body(&ents_web::router(state), "/account").await;
    assert!(
        body.contains("Signed in as the member below"),
        "the page states the session's identity"
    );
    assert!(
        body.contains(">joey</a>"),
        "the enrolled member's card renders"
    );

    let stranger = build_state(FixtureIdentity {
        name: "stranger",
        key: Keypair::from_seed(2),
    });
    let body = get_body(&ents_web::router(stranger), "/account").await;
    assert!(
        body.contains("not enrolled as a"),
        "an unenrolled key is stated, not hidden behind a signup form"
    );
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

/// `GET /files` renders the root `README` as a document card below the
/// listing -- re-homed from the old overview dashboard, so the repository
/// still introduces itself somewhere.
#[tokio::test]
async fn files_root_renders_the_readme_below_the_listing() {
    let dir = seed_repo(&[
        ("README.md", "# Welcome\n\nThe project overview.\n"),
        ("src/main.rs", "fn main() {}\n"),
    ]);
    let state = build_state_at(
        FixtureIdentity {
            name: "local-user",
            key: Keypair::from_seed(1),
        },
        dir.path().to_owned(),
    );
    let router = ents_web::router(state);

    let body = get_body(&router, "/files").await;
    assert!(
        body.contains("<h1>Welcome</h1>"),
        "the README renders as HTML, not raw markdown"
    );
    let listing = body.find("href=\"/files/src\"").expect("listing renders");
    let readme = body.find("<h1>Welcome</h1>").expect("README renders");
    assert!(listing < readme, "the README card sits below the listing");
}

/// The master-detail splits (`crate::pages::layout_split`): a blob view
/// renders a `.tree` sidebar with its own entry active and its siblings
/// listed; a commit page renders the compact history sidebar with the
/// viewed commit active; the issues page renders its list beside the
/// composer.
#[tokio::test]
async fn split_pages_render_a_sidebar_with_the_current_selection_active() {
    let dir = seed_repo(&[
        ("src/main.rs", "fn main() {}\n"),
        ("src/lib.rs", "pub fn f() {}\n"),
        ("README.md", "# hi\n"),
    ]);
    let oid = head_oid(dir.path());
    let state = build_state_at(
        FixtureIdentity {
            name: "local-user",
            key: Keypair::from_seed(1),
        },
        dir.path().to_owned(),
    );
    let router = ents_web::router(state.clone());
    let issue_id = seed_issue(&router, &state, "Split the panes", "open", "", "").await;

    let blob = get_body(&router, "/files/src/main.rs").await;
    assert!(blob.contains("class=\"tree\""), "the blob page splits");
    assert!(
        blob.contains(">main.rs</a>") && blob.contains("active"),
        "the viewed blob's own entry renders in the sidebar"
    );
    assert!(
        blob.contains("href=\"/files/src/lib.rs\""),
        "its sibling entries render beside it"
    );
    assert!(
        blob.contains("class=\"pane\""),
        "the content sits in a pane"
    );

    let commit = get_body(&router, &format!("/commit/{oid}")).await;
    assert!(commit.contains("class=\"tree\""), "the commit page splits");
    assert!(
        commit.contains(&format!("class=\"active\" href=\"/commit/{oid}\"")),
        "the viewed commit highlights in the history sidebar"
    );

    let issues = get_body(&router, "/issues").await;
    assert!(issues.contains("class=\"tree\""), "the issues page splits");
    assert!(
        issues.contains("Split the panes") && issues.contains("Open an Issue"),
        "the list and the composer render side by side"
    );
    let detail = get_body(&router, &format!("/issues/{issue_id}")).await;
    assert!(
        detail.contains(&format!(
            "class=\"side-row active\" href=\"/issues/{issue_id}\""
        )),
        "the viewed issue highlights in the sidebar"
    );
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
/// family truthfully: Dashboard, Code, Review, Issues, Threads, then the
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
        "/agents",
        "/comments",
        "/meta",
        "/account",
    ] {
        assert!(
            overview.contains(&format!("href=\"{href}\"")),
            "the rail links {href}"
        );
    }
    for label in ["Dashboard", "Code", "Review", "Issues", "Agents", "Threads"] {
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

/// `GET /members` and `GET /members/{username}` render an identity card
/// per member -- username prominent, the key type as a badge, the key
/// material truncated through the middle with the full line behind a
/// details toggle -- never the generic entity table an SSH key's base64
/// body used to shred.
#[tokio::test]
async fn members_pages_render_an_identity_card_per_member() {
    let refs = MemRefStore::default();
    let objects = ObjectStore::default();
    let key = Keypair::from_seed(1);
    let full_key = key.public_openssh();
    enroll_member(
        &refs,
        &objects,
        "jdc",
        &key,
        Provenance::AdminRegistered,
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

    let list = get_body(&router, "/members").await;
    assert!(list.contains("member-card"), "the identity card renders");
    assert!(
        list.contains("class=\"key-badge\"") && list.contains("ssh-ed25519"),
        "the key type badges"
    );
    assert!(
        list.contains('\u{2026}'),
        "the key material truncates through the middle"
    );
    assert!(
        list.contains("full key") && list.contains(&full_key),
        "the full key line stays one details toggle away"
    );
    assert!(
        !list.contains("entity-list"),
        "the generic table no longer renders here"
    );

    let show = get_body(&router, "/members/jdc").await;
    assert!(show.contains("member-card"));
    assert!(show.contains("class=\"key-badge\""));
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
        page.contains("class=\"verdict verdict-request-changes\""),
        "the verdict renders prominently, colored by its value"
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

/// The commit page's Checks card (`model.result-identity`,
/// `model.result-taxonomy`): a recorded result whose stored target names
/// the shown commit renders as a status chip and its effect's link --
/// one row per taxonomy value here -- a self-run mirror row additionally
/// names its member, and a result targeting a different commit stays off
/// the page (the tree's own `target` field is what is matched, not the
/// refname).
#[tokio::test]
async fn commit_page_lists_recorded_results_as_checks() {
    let dir = seed_repo(&[("src/main.rs", "fn main() {}\n")]);
    let oid = head_oid(dir.path());
    let refs = MemRefStore::default();
    let objects = ObjectStore::default();
    record_result(&refs, &objects, "unit", &oid, Status::Pass, None, 1_000);
    record_result(&refs, &objects, "lint", &oid, Status::Fail, None, 1_001);
    record_result(&refs, &objects, "deploy", &oid, Status::Error, None, 1_002);
    // Same effect, different target commit: filtered out by the stored
    // target field.
    record_result(
        &refs,
        &objects,
        "unit",
        "aaaaaaaa",
        Status::Pass,
        None,
        1_003,
    );
    // A self-run mirror row names the member that ran it.
    let member = MemberId::new("joey");
    let target = gix::ObjectId::from_hex(oid.as_bytes()).expect("head oid is hex");
    let self_ref = ents_model::namespace::self_result_ref(&member, "unit", &oid).expect("valid");
    let mirror = ResultRecord::new("unit", target, Status::Pass);
    write_meta_entity(&refs, &objects, self_ref, &mirror, None, 1_004);

    let state = Arc::new(AppState::new(
        Box::new(refs),
        objects,
        Box::new(NullEventSink),
        Mode::Advisory,
        Box::new(FixtureIdentity {
            name: "local-user",
            key: Keypair::from_seed(1),
        }),
        dir.path().to_owned(),
    ));
    let router = ents_web::router(state);

    let body = get_body(&router, &format!("/commit/{oid}")).await;
    assert!(body.contains("Checks"), "the Checks card renders");
    for (chip, effect) in [
        ("status-pass", "unit"),
        ("status-fail", "lint"),
        ("status-error", "deploy"),
    ] {
        assert!(
            body.contains(chip),
            "the {effect} row carries its {chip} chip"
        );
        assert!(
            body.contains(&format!("/effects/{effect}")),
            "the {effect} row links to its effect page"
        );
    }
    assert!(
        body.contains("self-run by joey"),
        "the mirror row names its member"
    );
    // The chip class appears once in the canonical unit row and once in
    // the self-run mirror row -- never for the other commit's result.
    assert_eq!(
        body.matches("status-pass").count(),
        2,
        "the other commit's result stays off the page"
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

// ---------------------------------------------------------------------
// roots.web-signin: the hosted sign-in surface, driven exactly as the
// CLI and a browser would drive it, still with no socket anywhere.
// ---------------------------------------------------------------------

/// Sign `payload` under the *login* namespace with seed `seed`'s key --
/// the same deterministic key `Keypair::from_seed(seed)` wraps, rebuilt
/// here because `Keypair::sign` deliberately signs only git's own commit
/// namespace.
fn login_sign(seed: u8, payload: &[u8]) -> String {
    use ssh_key::private::{Ed25519Keypair, KeypairData};
    use ssh_key::{HashAlg, LineEnding, PrivateKey};
    let pair = Ed25519Keypair::from_seed(&[seed; 32]);
    let key = PrivateKey::new(KeypairData::from(pair), "test").expect("well-formed");
    key.sign(ents_web::auth::LOGIN_NAMESPACE, HashAlg::Sha512, payload)
        .expect("signing is infallible")
        .to_pem(LineEnding::LF)
        .expect("renders")
}

/// Percent-encode a form value (the armored signature carries newlines,
/// `+`, `/`, and `=`, every one of which is significant to a form body).
fn urlencode(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                char::from(b).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// A sign-in-required state over `refs`/`objects`, host `ents.test` --
/// the hosted composition root's shape (`roots.single-node-hosted`),
/// minus the real repo.
fn build_signin_state(
    identity: FixtureIdentity,
    refs: MemRefStore,
    objects: ObjectStore,
) -> Arc<AppState<ObjectStore>> {
    Arc::new(
        AppState::new(
            Box::new(refs),
            objects,
            Box::new(NullEventSink),
            Mode::Advisory,
            Box::new(identity),
            std::env::temp_dir(),
        )
        .with_access(ents_web::state::AccessPolicy::SignInRequired(
            ents_web::state::Realm {
                host: "ents.test".to_owned(),
                challenges: ents_web::auth::ChallengeStore::default(),
            },
        )),
    )
}

/// GET /login with `cookie`, returning the fresh code the page displays.
async fn fetch_login_code(router: &axum::Router, cookie: &str) -> String {
    let response = router
        .clone()
        .oneshot(
            Request::get("/login")
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert_eq!(response.status(), StatusCode::OK);
    let body = String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes()
            .to_vec(),
    )
    .expect("utf8");
    let after = body
        .split("ents.test ")
        .nth(1)
        .expect("the page displays the login command");
    after.chars().take(9).collect()
}

/// Complete a challenge for `code` as seed `seed`'s key, returning the
/// response.
async fn complete_challenge(
    router: &axum::Router,
    code: &str,
    seed: u8,
    host: &str,
) -> axum::response::Response {
    let challenge = router
        .clone()
        .oneshot(
            Request::get(format!("/login/challenge/{code}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert_eq!(challenge.status(), StatusCode::OK, "challenge fetch");
    let text = String::from_utf8(
        challenge
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes()
            .to_vec(),
    )
    .expect("utf8");
    let nonce = text
        .lines()
        .find_map(|line| line.strip_prefix("nonce="))
        .expect("a nonce line");

    let normalized = ents_web::auth::normalize_code(code);
    let payload = ents_web::auth::challenge_payload(host, &normalized, nonce);
    let signature = login_sign(seed, payload.as_bytes());
    let public_key = Keypair::from_seed(seed).public_openssh();
    let form = format!(
        "public_key={}&signature={}",
        urlencode(&public_key),
        urlencode(&signature)
    );
    router
        .clone()
        .oneshot(
            Request::post(format!("/login/challenge/{code}"))
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(form))
                .expect("request"),
        )
        .await
        .expect("in-process call")
}

/// Rewrite `username`'s member record as revoked, directly against
/// `state`'s own stores -- the commit is staged via a scratch ref store
/// (`write_meta_entity` wants a concrete `MemRefStore`) and applied to
/// the live one through the trait's own CAS transaction.
fn revoke_member(state: &AppState<ObjectStore>, username: &str, key: &Keypair, seconds: i64) {
    let scratch = MemRefStore::default();
    let name = ents_model::namespace::member_ref(&MemberId::new(username)).expect("valid");
    let mut member = ents_model::Member::new(
        MemberId::new(username),
        key.public_openssh(),
        Provenance::AdminRegistered,
    );
    member.state = ents_model::MemberState::Revoked;
    let oid = write_meta_entity(
        &scratch,
        &*state.objects(),
        name.clone(),
        &member,
        Some(&Keypair::from_seed(9)),
        seconds,
    );
    state
        .refs
        .transaction(&[gix_ref_store::RefEdit {
            name,
            expected: gix_ref_store::Expected::Any,
            new: Some(oid),
        }])
        .expect("applies");
}

#[tokio::test]
// @relation(roots.web-signin, scope=function, role=Verifies)
async fn the_login_surface_is_unrouted_under_trusted() {
    let state = build_state(FixtureIdentity {
        name: "local-user",
        key: Keypair::from_seed(1),
    });
    let router = ents_web::router(Arc::clone(&state));
    for path in ["/login", "/login/challenge/ABCD2345"] {
        let response = router
            .clone()
            .oneshot(Request::get(path).body(Body::empty()).expect("request"))
            .await
            .expect("in-process call");
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "GET {path}");
    }
}

#[tokio::test]
// @relation(roots.web-signin, scope=function, role=Verifies)
async fn the_cli_challenge_flow_signs_the_browser_session_in() {
    let refs = MemRefStore::default();
    let objects = ObjectStore::default();
    let joey = Keypair::from_seed(7);
    enroll_member(
        &refs,
        &objects,
        "joey",
        &joey,
        Provenance::AdminRegistered,
        100,
    );
    let state = build_signin_state(
        FixtureIdentity {
            name: "server",
            key: Keypair::from_seed(9),
        },
        refs,
        objects,
    );
    let router = ents_web::router(Arc::clone(&state));

    let (cookie, _csrf) = session_cookie_and_csrf(&router, &state, "/").await;
    let code = fetch_login_code(&router, &cookie).await;

    let response = complete_challenge(&router, &code, 7, "ents.test").await;
    assert_eq!(response.status(), StatusCode::OK, "sign-in completes");

    // The browser's next look at /login reads as signed in.
    let session_id = cookie
        .split(';')
        .next()
        .expect("segment")
        .split_once('=')
        .expect("name=value")
        .1
        .to_owned();
    let member = state
        .sessions
        .get(&session_id)
        .expect("held")
        .member
        .expect("signed in");
    assert_eq!(member.username, "joey");

    // And a second post of the same code finds it consumed -- posted
    // directly, since the challenge fetch itself now correctly 404s.
    let replay = router
        .clone()
        .oneshot(
            Request::post(format!("/login/challenge/{code}"))
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("public_key=x&signature=y"))
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert_eq!(replay.status(), StatusCode::NOT_FOUND, "single-use");
}

#[tokio::test]
// @relation(roots.web-signin, scope=function, role=Verifies)
async fn a_wrong_host_signature_or_foreign_key_is_refused() {
    let refs = MemRefStore::default();
    let objects = ObjectStore::default();
    let joey = Keypair::from_seed(7);
    enroll_member(
        &refs,
        &objects,
        "joey",
        &joey,
        Provenance::AdminRegistered,
        100,
    );
    let state = build_signin_state(
        FixtureIdentity {
            name: "server",
            key: Keypair::from_seed(9),
        },
        refs,
        objects,
    );
    let router = ents_web::router(Arc::clone(&state));
    let (cookie, _csrf) = session_cookie_and_csrf(&router, &state, "/").await;

    // A signature over another deployment's host does not verify here.
    let code = fetch_login_code(&router, &cookie).await;
    let response = complete_challenge(&router, &code, 7, "evil.example").await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // An unenrolled key's valid signature is refused as not a member.
    let code = fetch_login_code(&router, &cookie).await;
    let response = complete_challenge(&router, &code, 3, "ents.test").await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // A revoked member's key is refused the same way.
    revoke_member(&state, "joey", &joey, 200);
    let code = fetch_login_code(&router, &cookie).await;
    let response = complete_challenge(&router, &code, 7, "ents.test").await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
// @relation(roots.web-signin, roots.web-session, scope=function, role=Verifies)
async fn mutations_require_a_live_signed_in_member_and_csrf_still_gates() {
    let refs = MemRefStore::default();
    let objects = ObjectStore::default();
    let joey = Keypair::from_seed(7);
    enroll_member(
        &refs,
        &objects,
        "joey",
        &joey,
        Provenance::AdminRegistered,
        100,
    );
    let state = build_signin_state(
        FixtureIdentity {
            name: "server",
            key: Keypair::from_seed(9),
        },
        refs,
        objects,
    );
    let router = ents_web::router(Arc::clone(&state));
    let (cookie, csrf) = session_cookie_and_csrf(&router, &state, "/").await;

    let post_account = |form: String, with_cookie: bool, accept_html: bool| {
        let mut request = Request::post("/account")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if with_cookie {
            request = request.header(header::COOKIE, cookie.clone());
        }
        if accept_html {
            request = request.header(header::ACCEPT, "text/html");
        }
        router
            .clone()
            .oneshot(request.body(Body::from(form)).expect("request"))
    };

    // Anonymous: a browser-shaped POST redirects to /login, a bare one
    // gets 401.
    let form = format!("member=joey&login=j@ents.test&csrf={csrf}");
    let response = post_account(form.clone(), true, true).await.expect("call");
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        response.headers().get(header::LOCATION).expect("location"),
        "/login"
    );
    let response = post_account(form.clone(), true, false).await.expect("call");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // Sign in, then the same POST passes the middleware -- and CSRF is
    // still enforced on top of it.
    let code = fetch_login_code(&router, &cookie).await;
    let signed_in = complete_challenge(&router, &code, 7, "ents.test").await;
    assert_eq!(signed_in.status(), StatusCode::OK);
    let response = post_account(form.clone(), true, true).await.expect("call");
    assert_eq!(response.status(), StatusCode::SEE_OTHER, "mutation lands");
    assert_eq!(
        response.headers().get(header::LOCATION).expect("location"),
        "/account"
    );

    // The landed commit is authored by the member and committed by the
    // server identity -- "joey via the web"
    // (receive.attributed-author, roots.web-signing).
    {
        use gix_object::Find as _;
        let account_ref: gix::refs::FullName = ents_model::namespace::ACCOUNT_REF
            .try_into()
            .expect("valid");
        let tip = state
            .refs
            .get(account_ref.as_ref())
            .expect("readable")
            .expect("written");
        let mut buf = Vec::new();
        let objects = state.objects();
        let data = objects
            .try_find(&tip, &mut buf)
            .expect("readable")
            .expect("present");
        let commit = gix_object::CommitRef::from_bytes(data.data, tip.kind()).expect("parses");
        assert_eq!(
            commit.author().expect("author").name,
            "joey",
            "authored by the signed-in member"
        );
        assert_eq!(
            commit.committer().expect("committer").name,
            "server",
            "committed by the server identity"
        );
    }
    let bad_csrf = "member=joey&login=j@ents.test&csrf=not-the-token".to_owned();
    let response = post_account(bad_csrf, true, true).await.expect("call");
    assert_ne!(
        response.status(),
        StatusCode::SEE_OTHER,
        "a signed-in session still fails a wrong csrf token"
    );

    // Revoke joey mid-session: the next mutation is refused and the
    // session is signed out, not just bounced.
    revoke_member(&state, "joey", &joey, 300);
    let response = post_account(form, true, true).await.expect("call");
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        response.headers().get(header::LOCATION).expect("location"),
        "/login"
    );
    let session_id = cookie
        .split(';')
        .next()
        .expect("segment")
        .split_once('=')
        .expect("name=value")
        .1;
    assert!(
        state
            .sessions
            .get(session_id)
            .expect("held")
            .member
            .is_none(),
        "a revoked member is signed out, not left holding a dead session"
    );
}

// ---------------------------------------------------------------------
// `crate::pages::agents` (`docs/agent-sessions-plan.adoc`'s Phase 3).
// ---------------------------------------------------------------------

/// The actor signature every [`Identity`] the agent tests below build
/// carries -- shaped exactly like [`FixtureIdentity::actor`]. Only this
/// half factors into a helper: [`Identity::sign`] borrows its closure
/// (`ents_web::identity`'s own doc explains why: the closure must live in
/// the caller's own stack frame), so each test still builds its own
/// `Identity` literal around this, the same shape
/// `ents_web::receive_identity!` expands to.
fn fixture_actor(name: &'static str) -> gix::actor::Signature {
    gix::actor::Signature {
        name: name.into(),
        email: format!("{name}@ents.test").into(),
        time: gix::date::Time {
            seconds: 1_000,
            offset: 0,
        },
    }
}

/// `POST /agents`, seeding `prompt` as a fresh session's initial thread
/// turn -- what the agent tests below start a session through, exercising
/// the actual signed-write path (`ents_forge::agent::new`) rather than
/// poking the ref store directly. Asserts the write succeeded (a redirect
/// to the new session's own page) and returns its id, read from that
/// redirect's `Location` (`/agents/<id>`).
async fn seed_agent_via_web(
    router: &axum::Router,
    state: &AppState<ObjectStore>,
    prompt: &str,
) -> String {
    let (cookie, csrf) = session_cookie_and_csrf(router, state, "/agents").await;
    let form = format!(
        "prompt={}&base_ref=refs%2Fheads%2Fmain&model=claude-sonnet-5&review_policy=manual&csrf={csrf}",
        prompt.replace(' ', "+")
    );
    let response = router
        .clone()
        .oneshot(
            Request::post("/agents")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::COOKIE, cookie)
                .body(Body::from(form))
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert!(
        response.status().is_redirection(),
        "agent session create did not succeed: {:?}",
        response.status()
    );
    response
        .headers()
        .get(header::LOCATION)
        .expect("a successful agent create redirects to the new session")
        .to_str()
        .expect("ascii")
        .strip_prefix("/agents/")
        .expect("redirect targets /agents/<id>")
        .to_owned()
}

/// A freshly started session (no plan yet) renders as `planning` on both
/// the index and its own detail page, offers no confirm form, and never
/// leaks its seed prompt -- a thread turn like any other, never rendered
/// (`ents_forge::agent::AgentSession`'s own "never rendered" contract).
#[tokio::test]
// @relation(lens.parity, roots.web-signing, roots.web-session, scope=function, role=Verifies)
async fn agents_index_and_detail_render_a_freshly_started_session_as_planning_with_no_thread_leak()
{
    let state = build_state(FixtureIdentity {
        name: "filer",
        key: Keypair::from_seed(21),
    });
    let router = ents_web::router(state.clone());
    let id = seed_agent_via_web(&router, &state, "top secret task instructions").await;

    let index = get_body(&router, "/agents").await;
    assert!(index.contains(ents_forge::abbreviate_id(&id)));
    assert!(index.contains("planning"));
    assert!(
        !index.contains("top secret task instructions"),
        "thread content (the seed prompt) must never render on the list page"
    );

    let detail = get_body(&router, &format!("/agents/{id}")).await;
    assert!(detail.contains("planning"));
    assert!(detail.contains("No plan has been drafted yet."));
    assert!(
        !detail.contains("Confirm plan"),
        "no confirm form before a plan exists"
    );
    assert!(
        !detail.contains("top secret task instructions"),
        "thread content (the seed prompt) must never render on the detail page either"
    );
}

/// `roots.web-session`: starting a session is a state-changing route, so a
/// `POST /agents` with no CSRF field at all is rejected, and one with the
/// wrong token is a bad request -- the same gate every mutation in this
/// crate runs behind.
#[tokio::test]
// @relation(roots.web-session, scope=function, role=Verifies)
async fn agent_create_is_rejected_without_a_valid_csrf_token() {
    let state = build_state(FixtureIdentity {
        name: "filer",
        key: Keypair::from_seed(22),
    });
    let router = ents_web::router(Arc::clone(&state));

    let no_csrf = router
        .clone()
        .oneshot(
            Request::post("/agents")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(
                    "prompt=sneaky&base_ref=HEAD&model=claude-sonnet-5&review_policy=manual",
                ))
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert!(
        !no_csrf.status().is_success() && !no_csrf.status().is_redirection(),
        "a POST with no csrf field must not start a session"
    );

    let (cookie, _csrf) = session_cookie_and_csrf(&router, &state, "/agents").await;
    let wrong = router
        .oneshot(
            Request::post("/agents")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::COOKIE, cookie)
                .body(Body::from(
                    "prompt=sneaky&base_ref=HEAD&model=claude-sonnet-5&review_policy=manual&csrf=not-the-token",
                ))
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert_eq!(wrong.status(), StatusCode::BAD_REQUEST);
}

/// A plan drafted (via `ents_forge::agent::revise_plan`, standing in for
/// Phase 4's not-yet-built planning surface) puts a session in `awaiting
/// confirmation` with a one-tap Confirm form; posting it
/// (`POST /agents/{id}/confirm`) is a signed, CSRF-checked mutation that
/// binds the plan's hash and reads back as `queued`, with the confirm form
/// gone.
#[tokio::test]
// @relation(lens.parity, roots.web-signing, roots.web-session, scope=function, role=Verifies)
async fn confirming_an_awaiting_session_transitions_it_to_queued_through_a_signed_post() {
    let seed = 23u8;
    let state = build_state(FixtureIdentity {
        name: "filer",
        key: Keypair::from_seed(seed),
    });
    let router = ents_web::router(state.clone());
    let id = seed_agent_via_web(&router, &state, "draft a fix").await;

    let key = Keypair::from_seed(seed);
    let identity = Identity {
        actor: fixture_actor("filer"),
        author: None,
        sign: &|payload| key.sign(payload),
    };
    ents_forge::agent::revise_plan(
        state.refs.as_ref(),
        &*state.objects(),
        state.events.as_ref(),
        &id,
        "do the thing".to_owned(),
        &identity,
        state.mode,
    )
    .expect("revise_plan reaches an outcome");

    let detail = get_body(&router, &format!("/agents/{id}")).await;
    assert!(detail.contains("awaiting confirmation"));
    assert!(
        detail.contains("Confirm plan"),
        "a plan with no current confirm shows the one-tap confirm form"
    );

    let (cookie, csrf) = session_cookie_and_csrf(&router, &state, "/agents").await;
    let response = router
        .clone()
        .oneshot(
            Request::post(format!("/agents/{id}/confirm"))
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::COOKIE, cookie)
                .body(Body::from(format!("csrf={csrf}")))
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert!(
        response.status().is_redirection(),
        "{:?}",
        response.status()
    );

    let detail = get_body(&router, &format!("/agents/{id}")).await;
    assert!(detail.contains("queued"));
    assert!(
        !detail.contains("Confirm plan"),
        "a queued session no longer shows the confirm form"
    );
}

/// Every one of the six derived states
/// (`docs/agent-sessions-plan.adoc`'s Phase 3: planning, awaiting
/// confirmation, queued, running, done, failed) renders distinctly on the
/// index, the running session's own sandbox name renders verbatim, the
/// done/failed sessions show their result branch/failure detail -- and no
/// session's thread content ever appears on either the index or any
/// detail page, whatever its state.
#[tokio::test]
// @relation(lens.parity, scope=function, role=Verifies)
async fn agents_index_renders_every_derived_state_distinctly_and_never_leaks_thread_content() {
    let refs = MemRefStore::default();
    let objects = ObjectStore::default();

    let base = |member: &str, seconds: i64| {
        ents_forge::agent::SessionMeta::new(
            MemberId::new(member),
            seconds,
            "claude-sonnet-5",
            vec![],
            "refs/heads/main",
            ents_forge::agent::ReviewPolicy::Manual,
            None,
        )
    };

    let planning = ents_forge::agent::AgentSession {
        meta: base("jdc", 100),
        plan: None,
        confirm: None,
        thread: vec![b"thread marker planning".to_vec()],
    };
    write_meta_entity(
        &refs,
        &objects,
        ents_model::namespace::agent_session_ref("s-planning").expect("valid"),
        &planning,
        None,
        100,
    );

    let mut awaiting = ents_forge::agent::AgentSession {
        meta: base("jdc", 200),
        plan: Some("draft plan".to_owned()),
        confirm: None,
        thread: vec![b"thread marker awaiting".to_vec()],
    };
    awaiting.meta.status = ents_forge::agent::Status::Ready;
    write_meta_entity(
        &refs,
        &objects,
        ents_model::namespace::agent_session_ref("s-awaiting").expect("valid"),
        &awaiting,
        None,
        200,
    );

    let mut queued = ents_forge::agent::AgentSession {
        meta: base("jdc", 300),
        plan: Some("confirmed plan".to_owned()),
        confirm: None,
        thread: vec![b"thread marker queued".to_vec()],
    };
    queued.meta.status = ents_forge::agent::Status::Ready;
    let hash = queued.plan_hash().expect("plan set");
    queued.confirm = Some(ents_forge::agent::Confirm::new(
        hash,
        ents_forge::agent::ReviewPolicy::Manual,
    ));
    write_meta_entity(
        &refs,
        &objects,
        ents_model::namespace::agent_session_ref("s-queued").expect("valid"),
        &queued,
        None,
        300,
    );

    let mut running = ents_forge::agent::AgentSession {
        meta: base("jdc", 400),
        plan: Some("confirmed plan".to_owned()),
        confirm: None,
        thread: vec![b"thread marker running".to_vec()],
    };
    running.meta.status = ents_forge::agent::Status::Running;
    running.meta.sprite = Some("sprite-42".to_owned());
    running.meta.worker = Some(MemberId::new("worker-bot"));
    running.meta.started = Some(400);
    write_meta_entity(
        &refs,
        &objects,
        ents_model::namespace::agent_session_ref("s-running").expect("valid"),
        &running,
        None,
        400,
    );

    let mut done = ents_forge::agent::AgentSession {
        meta: base("jdc", 500),
        plan: Some("confirmed plan".to_owned()),
        confirm: None,
        thread: vec![b"thread marker done".to_vec()],
    };
    done.meta.status = ents_forge::agent::Status::Done;
    done.meta.result_branch = Some("agent/jdc/deadbeef".to_owned());
    done.meta.finished = Some(500);
    write_meta_entity(
        &refs,
        &objects,
        ents_model::namespace::agent_session_ref("s-done").expect("valid"),
        &done,
        None,
        500,
    );

    let mut failed = ents_forge::agent::AgentSession {
        meta: base("jdc", 600),
        plan: Some("confirmed plan".to_owned()),
        confirm: None,
        thread: vec![b"thread marker failed".to_vec()],
    };
    failed.meta.status = ents_forge::agent::Status::Failed(ents_forge::agent::FailureReason {
        detail: "sandbox died".to_owned(),
    });
    failed.meta.finished = Some(600);
    write_meta_entity(
        &refs,
        &objects,
        ents_model::namespace::agent_session_ref("s-failed").expect("valid"),
        &failed,
        None,
        600,
    );

    let state = build_state_with(
        FixtureIdentity {
            name: "filer",
            key: Keypair::from_seed(24),
        },
        refs,
        objects,
    );
    let router = ents_web::router(state.clone());

    let index = get_body(&router, "/agents").await;
    for label in [
        "planning",
        "awaiting confirmation",
        "queued",
        "running",
        "done",
        "failed",
    ] {
        assert!(index.contains(label), "the index shows the {label} state");
    }
    for marker in [
        "thread marker planning",
        "thread marker awaiting",
        "thread marker queued",
        "thread marker running",
        "thread marker done",
        "thread marker failed",
    ] {
        assert!(
            !index.contains(marker),
            "thread content must never render on the list page: {marker}"
        );
    }

    let running_detail = get_body(&router, "/agents/s-running").await;
    assert!(
        running_detail.contains("sprite-42"),
        "the sandbox name renders verbatim while running"
    );
    assert!(!running_detail.contains("thread marker running"));

    let done_detail = get_body(&router, "/agents/s-done").await;
    assert!(done_detail.contains("agent/jdc/deadbeef"));
    assert!(!done_detail.contains("thread marker done"));

    let failed_detail = get_body(&router, "/agents/s-failed").await;
    assert!(failed_detail.contains("sandbox died"));
    assert!(!failed_detail.contains("thread marker failed"));
}

/// The "result record" ref [`crate::pages::agents::show`] displays for a
/// terminal session is derived from the chain's own confirmed-and-queued
/// commit -- exactly what `git-ents::agent_worker::run_agent_exec` reads
/// as its own dispatched oid -- and reads back as "not yet recorded" until
/// a result actually lands at that derived ref, then as recorded once one
/// does.
#[tokio::test]
// @relation(lens.parity, effect.results-writeback, scope=function, role=Verifies)
async fn result_record_ref_is_derived_from_the_chains_own_confirmed_commit() {
    let seed = 25u8;
    let state = build_state(FixtureIdentity {
        name: "filer",
        key: Keypair::from_seed(seed),
    });
    let router = ents_web::router(state.clone());
    let id = seed_agent_via_web(&router, &state, "ship the fix").await;

    let key = Keypair::from_seed(seed);
    let identity = Identity {
        actor: fixture_actor("filer"),
        author: None,
        sign: &|payload| key.sign(payload),
    };

    ents_forge::agent::revise_plan(
        state.refs.as_ref(),
        &*state.objects(),
        state.events.as_ref(),
        &id,
        "do the thing".to_owned(),
        &identity,
        state.mode,
    )
    .expect("revise_plan reaches an outcome");
    ents_forge::agent::confirm(
        state.refs.as_ref(),
        &*state.objects(),
        state.events.as_ref(),
        &id,
        None,
        &identity,
        state.mode,
    )
    .expect("confirm reaches an outcome");

    let ref_name = ents_model::namespace::agent_session_ref(&id).expect("valid");
    let confirmed_tip = state
        .refs
        .get(ref_name.as_ref())
        .expect("read")
        .expect("confirmed tip exists");

    ents_forge::agent::claim(
        state.refs.as_ref(),
        &*state.objects(),
        state.events.as_ref(),
        &id,
        ents_forge::agent::ClaimAgentSession {
            worker: MemberId::new("worker-bot"),
            sprite: "sprite-9".to_owned(),
        },
        &identity,
        state.mode,
    )
    .expect("claim reaches an outcome");
    ents_forge::agent::finish(
        state.refs.as_ref(),
        &*state.objects(),
        state.events.as_ref(),
        &id,
        ents_forge::agent::FinishAgentSession {
            outcome: ents_forge::agent::FinishOutcome::Done,
            result_branch: Some("agent/filer/deadbeef".to_owned()),
            thread: vec![b"final log".to_vec()],
        },
        &identity,
        state.mode,
    )
    .expect("finish reaches an outcome");

    let expected_short = ents_effect::run::short_oid(confirmed_tip);
    let result_ref =
        ents_model::namespace::result_ref("agent-exec", &expected_short).expect("valid");
    let result_ref_display = result_ref.as_bstr().to_string();

    let before = get_body(&router, &format!("/agents/{id}")).await;
    assert!(before.contains(&result_ref_display));
    assert!(before.contains("not yet recorded"));

    let record = ResultRecord::new("agent-exec", confirmed_tip, Status::Pass);
    ents_receive::propose_entity(
        state.refs.as_ref(),
        &*state.objects(),
        state.events.as_ref(),
        result_ref.clone(),
        &record,
        &identity,
        "Record agent-exec result",
        state.mode,
    )
    .expect("reaches an outcome");

    let after = get_body(&router, &format!("/agents/{id}")).await;
    assert!(after.contains(&result_ref_display));
    assert!(!after.contains("not yet recorded"));
}

// ---------------------------------------------------------------------
// `crate::pages::agent_chat` (`docs/agent-sessions-plan.adoc`'s Phase 4:
// the laptop planning-chat page).
// ---------------------------------------------------------------------

/// `POST path` with a signed-in session's cookie and a form-encoded body,
/// returning the response -- the chat-page tests' own thin wrapper around
/// the same request shape [`agent_create_is_rejected_without_a_valid_csrf_token`]
/// builds inline, factored out here since every test below needs at least
/// one.
async fn post_form(
    router: &axum::Router,
    path: &str,
    cookie: &str,
    body: String,
) -> axum::http::Response<Body> {
    router
        .clone()
        .oneshot(
            Request::post(path)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::COOKIE, cookie)
                .body(Body::from(body))
                .expect("request"),
        )
        .await
        .expect("in-process call")
}

/// A freshly started (`planning`) session's detail page links to its own
/// planning-chat page, which itself renders the composer and the seeded
/// prompt (the one place a thread blob is deliberately rendered -- see
/// `crate::pages::agent_chat`'s own doc: it is the very page that wrote
/// it, for the member who wrote it).
#[tokio::test]
// @relation(lens.parity, scope=function, role=Verifies)
async fn agent_detail_links_to_chat_and_chat_renders_the_composer_and_prompt() {
    let state = build_state(FixtureIdentity {
        name: "filer",
        key: Keypair::from_seed(30),
    });
    let router = ents_web::router(state.clone());
    let id = seed_agent_via_web(&router, &state, "reproduce the flaky test").await;

    let detail = get_body(&router, &format!("/agents/{id}")).await;
    assert!(detail.contains(&format!("/agents/{id}/chat")));

    let chat = get_body(&router, &format!("/agents/{id}/chat")).await;
    assert!(chat.contains("data-agent-chat"));
    assert!(chat.contains("reproduce the flaky test"));
    assert!(!chat.contains("Reopen for planning"));
}

/// `POST /agents/{id}/chat` appends the member's message and the injected
/// `Planner`'s reply (`ents_web::planner::UnconfiguredPlanner`, the
/// default every composition root installs) in one commit, both of which
/// then render on a reload of the chat page.
#[tokio::test]
// @relation(lens.parity, roots.web-signing, roots.web-session, scope=function, role=Verifies)
async fn agent_chat_send_appends_the_turn_pair_and_renders_the_stub_planners_reply() {
    let state = build_state(FixtureIdentity {
        name: "filer",
        key: Keypair::from_seed(31),
    });
    let router = ents_web::router(state.clone());
    let id = seed_agent_via_web(&router, &state, "prompt").await;

    let (cookie, csrf) =
        session_cookie_and_csrf(&router, &state, &format!("/agents/{id}/chat")).await;
    let response = post_form(
        &router,
        &format!("/agents/{id}/chat"),
        &cookie,
        format!("message=what+should+the+plan+look+like%3F&csrf={csrf}"),
    )
    .await;
    assert!(response.status().is_redirection());

    let session =
        ents_forge::agent::show(state.refs.as_ref(), &*state.objects(), &id).expect("shows");
    assert_eq!(
        session.thread.len(),
        3,
        "prompt + user turn + assistant turn"
    );

    let chat = get_body(&router, &format!("/agents/{id}/chat")).await;
    assert!(chat.contains("what should the plan look like?"));
    assert!(chat.to_lowercase().contains("not configured"));
}

/// `roots.web-session`: `POST /agents/{id}/chat` is a state-changing route,
/// refused without a valid CSRF token exactly like every other mutation in
/// this crate.
#[tokio::test]
// @relation(roots.web-session, scope=function, role=Verifies)
async fn agent_chat_send_is_rejected_without_a_valid_csrf_token() {
    let state = build_state(FixtureIdentity {
        name: "filer",
        key: Keypair::from_seed(32),
    });
    let router = ents_web::router(state.clone());
    let id = seed_agent_via_web(&router, &state, "prompt").await;

    let (cookie, _csrf) =
        session_cookie_and_csrf(&router, &state, &format!("/agents/{id}/chat")).await;
    let response = post_form(
        &router,
        &format!("/agents/{id}/chat"),
        &cookie,
        "message=sneaky&csrf=wrong".to_owned(),
    )
    .await;
    assert!(!response.status().is_success() && !response.status().is_redirection());

    let session =
        ents_forge::agent::show(state.refs.as_ref(), &*state.objects(), &id).expect("shows");
    assert_eq!(
        session.thread.len(),
        1,
        "the bad-csrf message must not land"
    );
}

/// `docs/agent-sessions-plan.adoc`'s Phase 4 acceptance: once a session is
/// confirmed and queued, `POST /agents/{id}/chat` refuses rather than
/// silently un-queueing it; `POST /agents/{id}/reopen` is the explicit
/// un-queue that returns it to `planning`, after which chatting works
/// again.
#[tokio::test]
// @relation(scope=function, role=Verifies)
async fn agent_chat_refuses_a_queued_session_until_explicitly_reopened() {
    let seed = 33u8;
    let state = build_state(FixtureIdentity {
        name: "filer",
        key: Keypair::from_seed(seed),
    });
    let router = ents_web::router(state.clone());
    let id = seed_agent_via_web(&router, &state, "prompt").await;

    let key = Keypair::from_seed(seed);
    let identity = Identity {
        actor: fixture_actor("filer"),
        author: None,
        sign: &|payload| key.sign(payload),
    };
    ents_forge::agent::revise_plan(
        state.refs.as_ref(),
        &*state.objects(),
        state.events.as_ref(),
        &id,
        "do the thing".to_owned(),
        &identity,
        state.mode,
    )
    .expect("revises");
    ents_forge::agent::confirm(
        state.refs.as_ref(),
        &*state.objects(),
        state.events.as_ref(),
        &id,
        None,
        &identity,
        state.mode,
    )
    .expect("confirms");
    assert!(
        ents_forge::agent::show(state.refs.as_ref(), &*state.objects(), &id)
            .expect("shows")
            .queued()
    );

    let (cookie, csrf) =
        session_cookie_and_csrf(&router, &state, &format!("/agents/{id}/chat")).await;
    let refused = post_form(
        &router,
        &format!("/agents/{id}/chat"),
        &cookie,
        format!("message=let%27s+change+something&csrf={csrf}"),
    )
    .await;
    assert!(
        !refused.status().is_success() && !refused.status().is_redirection(),
        "a queued session must refuse a chat message rather than silently un-queue"
    );

    let queued_chat = get_body(&router, &format!("/agents/{id}/chat")).await;
    assert!(queued_chat.contains("Reopen for planning"));

    let reopened = post_form(
        &router,
        &format!("/agents/{id}/reopen"),
        &cookie,
        format!("csrf={csrf}"),
    )
    .await;
    assert!(reopened.status().is_redirection());

    let session =
        ents_forge::agent::show(state.refs.as_ref(), &*state.objects(), &id).expect("shows");
    assert_eq!(session.meta.status, ents_forge::agent::Status::Planning);
    assert!(session.confirm.is_none());

    let sent = post_form(
        &router,
        &format!("/agents/{id}/chat"),
        &cookie,
        format!("message=let%27s+change+something&csrf={csrf}"),
    )
    .await;
    assert!(
        sent.status().is_redirection(),
        "chatting works again once the session is reopened"
    );
}

/// `POST /agents/{id}/plan` commits the plan editor's text via
/// `ents_forge::agent::revise_plan`, transitioning the session to
/// `ready`/awaiting-confirmation -- the same commit path
/// `docs/agent-sessions-plan.adoc`'s Phase 4 names for both this chat page
/// and the headless `agent-plan` effect.
#[tokio::test]
// @relation(lens.parity, roots.web-signing, roots.web-session, scope=function, role=Verifies)
async fn agent_plan_commit_transitions_to_ready_awaiting_confirmation() {
    let state = build_state(FixtureIdentity {
        name: "filer",
        key: Keypair::from_seed(34),
    });
    let router = ents_web::router(state.clone());
    let id = seed_agent_via_web(&router, &state, "prompt").await;

    let (cookie, csrf) =
        session_cookie_and_csrf(&router, &state, &format!("/agents/{id}/chat")).await;
    let response = post_form(
        &router,
        &format!("/agents/{id}/plan"),
        &cookie,
        format!("plan=1.+read+the+test%0A2.+fix+it&csrf={csrf}"),
    )
    .await;
    assert!(response.status().is_redirection());

    let session =
        ents_forge::agent::show(state.refs.as_ref(), &*state.objects(), &id).expect("shows");
    assert_eq!(session.meta.status, ents_forge::agent::Status::Ready);
    assert!(session.awaiting_confirmation());

    let detail = get_body(&router, &format!("/agents/{id}")).await;
    assert!(detail.contains("Confirm plan"));
}
