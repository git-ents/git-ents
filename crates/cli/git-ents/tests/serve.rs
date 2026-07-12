//! Integration coverage for `git ents serve`'s wiring (`roots.local`):
//! [`git_ents::commands::serve::build_state`] reuses a real
//! [`LocalRoot`]'s own seams (the same loose-ref `RefStore` and odb every
//! other porcelain command uses), signs with the local user's own key
//! (`roots.web-signing`), and the resulting `ents-web` router carries no
//! git smart-HTTP surface — all driven in-process via
//! `tower::ServiceExt::oneshot`, no socket ever bound (`roots.web-agnostic`).
#![allow(clippy::expect_used, reason = "integration test")]

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use git_ents::commands::members;
use git_ents::root::LocalRoot;
use tower::ServiceExt as _;

/// `roots.local`: `git ents serve`'s state is built from an already-open
/// [`LocalRoot`] (never a second store), and the router it drives exposes
/// only the web UI — no `/info/refs`, no git wire protocol.
#[tokio::test]
// @relation(roots.local, roots.composition, scope=function, role=Verifies)
async fn serve_reuses_the_local_root_and_exposes_no_git_transport() {
    let fixture = common::Fixture::new(1);
    let root = LocalRoot::open(fixture.path()).expect("opens");
    members::add(&root, "jdc", None, Some(fixture.key_path.clone())).expect("bootstrap");

    let root = LocalRoot::open(fixture.path()).expect("reopen for serve");
    let state = git_ents::commands::serve::build_state(root, Some(fixture.key_path.clone()))
        .expect("builds state from the local root");
    let router = ents_web::router(state);

    let dashboard = router
        .clone()
        .oneshot(Request::get("/").body(Body::empty()).expect("request"))
        .await
        .expect("in-process call");
    assert_eq!(dashboard.status(), StatusCode::OK);

    let members_page = router
        .clone()
        .oneshot(
            Request::get("/members")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert_eq!(members_page.status(), StatusCode::OK);

    let smart_http = router
        .oneshot(
            Request::get("/info/refs?service=git-upload-pack")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("in-process call");
    assert_eq!(
        smart_http.status(),
        StatusCode::NOT_FOUND,
        "git ents serve must never expose git's own smart-HTTP transport"
    );
}

/// `roots.web-signing`: the identity chip shows the signer's own enrolled
/// member username, resolved via `commands::members::find_by_key` (the
/// same key-match loop `git ents members check` runs) -- not
/// `actor().name`, which is a fixed `"git-ents"` commit-author wordmark
/// that would otherwise just duplicate the site logo next to it.
#[tokio::test]
// @relation(roots.web-signing, scope=function, role=Verifies)
async fn serve_identity_chip_shows_the_signers_enrolled_member_username() {
    let fixture = common::Fixture::new(1);
    let root = LocalRoot::open(fixture.path()).expect("opens");
    members::add(&root, "jdc", None, Some(fixture.key_path.clone())).expect("bootstrap");

    let root = LocalRoot::open(fixture.path()).expect("reopen for serve");
    let state = git_ents::commands::serve::build_state(root, Some(fixture.key_path.clone()))
        .expect("builds state from the local root");
    let router = ents_web::router(state);

    let response = router
        .oneshot(Request::get("/").body(Body::empty()).expect("request"))
        .await
        .expect("in-process call");
    assert_eq!(response.status(), StatusCode::OK);
    let body = String::from_utf8(
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body")
            .to_vec(),
    )
    .expect("utf8 html");
    assert!(
        body.contains(r#"class="id-chip" href="/account">jdc</a>"#),
        "the id-chip must show the enrolled member's own username: {body}"
    );
}

/// A signer whose key names no enrolled member falls back to a short key
/// fingerprint for the identity chip -- never `actor()`'s `"git-ents"`
/// wordmark, which would silently duplicate the site logo.
#[tokio::test]
// @relation(roots.web-signing, scope=function, role=Verifies)
async fn serve_identity_chip_falls_back_to_a_fingerprint_when_unenrolled() {
    let fixture = common::Fixture::new(2);
    let root = LocalRoot::open(fixture.path()).expect("opens");
    let state = git_ents::commands::serve::build_state(root, Some(fixture.key_path.clone()))
        .expect("builds state from the local root");
    let router = ents_web::router(state);

    let response = router
        .oneshot(Request::get("/").body(Body::empty()).expect("request"))
        .await
        .expect("in-process call");
    assert_eq!(response.status(), StatusCode::OK);
    let body = String::from_utf8(
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body")
            .to_vec(),
    )
    .expect("utf8 html");
    let chip = body
        .split(r#"class="id-chip" href="/account">"#)
        .nth(1)
        .and_then(|rest| rest.split("</a>").next())
        .expect("id-chip renders");
    assert_ne!(
        chip, "git-ents",
        "an unenrolled key must never fall back to the commit-author wordmark"
    );
    assert!(!chip.is_empty(), "the fingerprint fallback is never blank");
}
