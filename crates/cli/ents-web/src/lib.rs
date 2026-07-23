//! `ents-web`: the web UI (`docs/development-plan.adoc`, phase 7) --
//! a second leaf, sibling to `git-ents`, in the layering
//! `docs/abstractions.adoc` states (`substrate -> kernel -> {forge, kiln}
//! -> {git-ents, ents-web}`).
//!
//! This crate's one responsibility is rendering the kernel's and every
//! installed package's own state as HTML, and accepting signed,
//! CSRF-checked mutations back -- never a second copy of forge or kiln
//! business logic. Every page is a thin caller into `ents-model`,
//! `ents-anchor`, `ents-query`, `ents-receive`, `ents-forge`, or
//! `ents-kiln`, exactly as `git-ents`'s own `commands` modules are thin
//! callers into the same crates ([`crate::pages`]'s own module doc draws
//! the line between the generic, reflection-driven pages and the
//! legitimate custom ones).
//!
//! # Deployment-agnostic by construction (`roots.web-agnostic`)
//!
//! Nothing in this crate binds a socket except [`serve_on`], and nothing
//! upstream of it assumes one exists: `router()` alone builds a complete,
//! in-process `tower::Service` a caller can drive via
//! `tower::ServiceExt::oneshot` with no network transport at all -- the
//! same shape an in-process webview embedding would drive a request
//! through. See [`identity::SigningIdentity`]'s own doc for the other half
//! of this requirement: the signing identity a mutation is signed with is
//! always injected by the composition root, never resolved by this crate
//! itself.
//!
//! # What this crate does not expose (`roots.local`)
//!
//! There is no `/info/refs`, no `git-upload-pack`/`git-receive-pack`
//! route, and no code path that shells to `git` as a smart-HTTP backend
//! anywhere in `router()`'s route table. `git ents serve`'s own doc
//! (`git-ents`'s `commands::serve`) states why: the local root's existing
//! wiring already serves git's own transport for the test-harness case
//! (`roots.worktree-update`); this crate adds only the web UI on top of
//! it, on loopback, never a second git-serving surface.
//!
//! # Spec coverage
//!
//! From `docs/spec/roots.adoc`:
//!
//! - `roots.local` -- this crate's route table carries no git
//!   smart-HTTP surface; `git-ents`'s own `serve` command reuses
//!   `LocalRoot`'s existing seams and binds loopback only (see that
//!   crate's `commands::serve` module).
//! - `roots.web-signing`, `roots.web-agnostic` -- [`identity::SigningIdentity`].
//! - `roots.web-session` -- [`session::SessionStore`], and
//!   `pages::require_csrf` on every state-changing route.
//!
//! `roots.path-validation` and `roots.fetch-auth` are out of scope for
//! this crate: both describe `git-ents-server`'s multi-repository hosted
//! root (phase 8) -- "reject a path that would escape the data
//! directory, nest inside an existing repository, or collide with a
//! non-repository namespace directory" and "private-repository access...
//! out of scope for v1" both presuppose a data directory holding more
//! than one repository, which does not exist until that phase. This
//! crate's composition root always already has exactly one, already-open
//! repository.
//!
//! # Examples
//!
//! Driving a full request through this crate with no socket bound at all
//! (`roots.web-agnostic`'s in-process case) -- see `tests/router.rs` for
//! the full-fixture version of this same shape, wired against a real
//! signed member.
//!
//! ```
//! use std::sync::Arc;
//!
//! use ents_web::identity::SigningIdentity;
//! use ents_web::state::AppState;
//! use ents_receive::{Mode, NullEventSink};
//! use ents_testutil::ObjectStore;
//! use gix_ref_store::LooseRefStore;
//! use http_body_util::BodyExt as _;
//! use tower::ServiceExt as _;
//!
//! struct Fixture;
//! impl SigningIdentity for Fixture {
//!     fn actor(&self) -> gix::actor::Signature {
//!         gix::actor::Signature {
//!             name: "fixture".into(), email: "fixture@ents.test".into(),
//!             time: gix::date::Time { seconds: 0, offset: 0 },
//!         }
//!     }
//!     fn sign(&self, _payload: &[u8]) -> String { String::new() }
//!     fn public_openssh(&self) -> String { "ssh-ed25519 AAAA... fixture".to_owned() }
//! }
//!
//! # let runtime = tokio::runtime::Runtime::new().expect("runtime");
//! # runtime.block_on(async {
//! let dir = tempfile::tempdir().expect("tempdir");
//! gix::init(dir.path()).expect("init");
//! let refs = LooseRefStore::open(dir.path()).expect("opens");
//! let objects = ObjectStore::default();
//! let state = Arc::new(AppState::new(
//!     Box::new(refs), objects, Box::new(NullEventSink), Mode::Advisory,
//!     Box::new(Fixture), dir.path().to_owned(),
//! ));
//! let router = ents_web::router(state);
//! let response = router
//!     .oneshot(axum::http::Request::get("/").body(axum::body::Body::empty()).expect("request"))
//!     .await
//!     .expect("in-process call");
//! assert_eq!(response.status(), axum::http::StatusCode::OK);
//! # });
//! ```

pub(crate) mod asciidoc;
pub(crate) mod assets;
pub mod auth;
pub(crate) mod editor;
pub mod error;
pub mod form;
pub mod identity;
pub(crate) mod markdown;
pub mod pages;
pub mod render;
pub mod router;
pub mod session;
pub mod state;

pub use error::{Error, Result};
pub use router::{bind, router, serve_on};
