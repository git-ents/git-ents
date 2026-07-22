//! The web frontend's own handle onto the four composition-root seams
//! (`roots.composition`), generic over only the object store: `refs` and
//! `events` are already used as trait objects everywhere in this codebase
//! (`git_ents::root::LocalRoot` passes `&root.refs` where `&dyn RefStore`
//! is expected; `ents_forge::comment::add` takes `events: &dyn
//! ents_receive::EventSink` directly), so [`AppState`] holds them boxed
//! rather than introducing a type parameter this crate has no other use
//! for. The object store stays a type parameter `O` because every mutation
//! path (`ents_receive::propose_entity`) takes it as `&(impl
//! gix_object::Find + gix_object::Write)`, generic, never `dyn` --
//! matching that established shape rather than inventing a private
//! object-store trait (`arch.no-object-store-trait`).
//!
//! `objects` is held behind a [`std::sync::Mutex`] rather than bare `O`:
//! axum requires its `State` to be `Sync` (so it can be shared across
//! however many worker tasks accept connections), but neither this
//! crate's real composition-root object store nor its test fixture
//! (`ents_testutil::ObjectStore`, used by every test in this crate) is
//! `Sync` on its own -- the fixture's internal `RefCell` makes that
//! concrete, but the same caution applies to any future object-store
//! implementation this crate is handed, since nothing about
//! `gix_object::Find`/`Write` requires an implementation to be safe for
//! concurrent access. Serializing access behind one mutex is the right
//! choice for this crate regardless: a web admin UI's request volume is
//! not a throughput target `roots.adoc` names anywhere.

use std::path::PathBuf;
use std::sync::Mutex;

use ents_receive::{EventSink, Mode};
use gix_ref_store::RefStore;

use crate::auth::ChallengeStore;
use crate::identity::SigningIdentity;
use crate::planner::{Planner, UnconfiguredPlanner};
use crate::session::SessionStore;

/// Who may mutate through this deployment's web UI — injected by the
/// composition root, exactly as the signing identity is
/// (`roots.web-agnostic`): this crate branches on the injected policy's
/// value, never on where it is running (`arch.no-hosted-branch`'s
/// spirit).
// @relation(roots.web-signin, scope=type)
pub enum AccessPolicy {
    /// Every session mutates as the injected identity — the local root
    /// (`roots.local`), where the operator's own key *is* the identity
    /// and no sign-in surface exists (`roots.web-signin`).
    Trusted,
    /// Anonymous sessions browse read-only; a mutation requires a
    /// session signed in as an enrolled, active member
    /// (`roots.web-signin`) — the hosted root.
    SignInRequired(Realm),
}

/// What a sign-in-required deployment knows about itself: the canonical
/// external host bound into every challenge payload
/// ([`crate::auth::challenge_payload`]), and the outstanding challenges.
pub struct Realm {
    /// The host a member addresses this deployment as, e.g.
    /// `git.ents.cloud` — a signature is bound to it, so one minted for
    /// this realm verifies nowhere else (`roots.web-signin`).
    pub host: String,
    /// Outstanding sign-in challenges, memory-only like the sessions.
    pub challenges: ChallengeStore,
}

/// Everything a page handler needs: the four composition-root seams, the
/// gate policy in force, the repository's working-tree path (comment
/// anchoring resolves paths against it), and the in-memory session store
/// (`roots.web-session`).
///
/// Built once per `ents_web::serve`/`ents_web::router` call, by whichever
/// composition root is wiring this crate in -- never constructed inside a
/// page handler itself (`roots.config-isolation`'s spirit: every seam
/// arrives already chosen).
pub struct AppState<O> {
    /// The ref store, as the same trait-object shape every mutation
    /// primitive in this codebase already takes it.
    pub refs: Box<dyn RefStore>,
    /// The object store, mutex-serialized (see this module's own doc for
    /// why). A type parameter, not `dyn`, so every existing
    /// `propose_entity`/`comment::add`/`toolchain::import` call compiles
    /// unchanged against a lock guard's deref.
    objects: Mutex<O>,
    /// The event sink obligations are enqueued to on a push
    /// (`receive.event-sink`) -- a local deployment injects a null sink
    /// (`roots.local`), matching `git ents`'s own CLI commands.
    pub events: Box<dyn EventSink>,
    /// The gate policy this deployment runs under (`roots.local`:
    /// advisory; a future hosted `ents-web` wiring: mandatory).
    pub mode: Mode,
    /// The signing identity every mutation page signs on behalf of
    /// (`roots.web-signing`, `roots.web-agnostic`).
    pub identity: Box<dyn SigningIdentity>,
    /// The repository's own path, for anchoring operations
    /// (`ents_forge::comment`) that need to open the working tree
    /// directly.
    pub path: PathBuf,
    /// In-memory web sessions (`roots.web-session`).
    pub sessions: SessionStore,
    /// Who may mutate here (`roots.web-signin`): [`AccessPolicy::Trusted`]
    /// unless the composition root said otherwise via
    /// [`AppState::with_access`].
    pub access: AccessPolicy,
    /// The planning-chat page's LLM seam
    /// (`docs/agent-sessions-plan.adoc`'s Phase 4): [`UnconfiguredPlanner`]
    /// unless the composition root said otherwise via
    /// [`AppState::with_planner`].
    pub planner: Box<dyn Planner>,
}

impl<O> AppState<O> {
    /// Build a state from already-wired seams -- the one constructor every
    /// composition root calls, and the only place a fresh
    /// [`SessionStore`] is created.
    pub fn new(
        refs: Box<dyn RefStore>,
        objects: O,
        events: Box<dyn EventSink>,
        mode: Mode,
        identity: Box<dyn SigningIdentity>,
        path: PathBuf,
    ) -> Self {
        Self {
            refs,
            objects: Mutex::new(objects),
            events,
            mode,
            identity,
            path,
            sessions: SessionStore::default(),
            access: AccessPolicy::Trusted,
            planner: Box::new(UnconfiguredPlanner),
        }
    }

    /// Replace the default [`AccessPolicy::Trusted`] — the hosted
    /// composition root's one extra wiring step
    /// (`roots.single-node-hosted`, `roots.web-signin`). A consuming
    /// builder rather than a constructor parameter so every existing
    /// `new` caller (every `Trusted` deployment and test fixture) stays
    /// untouched.
    #[must_use]
    pub fn with_access(mut self, access: AccessPolicy) -> Self {
        self.access = access;
        self
    }

    /// Replace the default [`UnconfiguredPlanner`] — a real Planner is
    /// wired the same consuming-builder way once one exists (per-member
    /// credentials, `docs/agent-sessions-plan.adoc`'s Phase 6), so every
    /// existing `new` caller stays untouched until a composition root
    /// opts in.
    #[must_use]
    pub fn with_planner(mut self, planner: Box<dyn Planner>) -> Self {
        self.planner = planner;
        self
    }

    /// Lock the object store for the duration of one request.
    ///
    /// Poisoning recovers rather than propagating (mirrors
    /// `SessionStore`'s identical reasoning): an earlier request
    /// panicking mid-write already unwound that request's own response;
    /// refusing every subsequent request forever would be strictly worse
    /// than reusing the store as-is.
    pub fn objects(&self) -> std::sync::MutexGuard<'_, O> {
        self.objects
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
