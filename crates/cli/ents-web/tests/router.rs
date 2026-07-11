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
use ents_model::{Account, MemberId};
use ents_receive::{Mode, NullEventSink};
use ents_testutil::{Keypair, MemRefStore, ObjectStore};
use ents_web::identity::SigningIdentity;
use ents_web::state::AppState;
use gix::bstr::ByteSlice as _;
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
