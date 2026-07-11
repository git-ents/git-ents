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
