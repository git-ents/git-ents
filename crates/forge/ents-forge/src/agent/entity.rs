//! The AgentSession entity: a mandatory plan-and-confirm ceremony around a
//! headless agent run, living at its own `refs/meta/agent-sessions/<id>` ref
//! (`namespace::agent_session_ref`) — Phase 1 of
//! `docs/agent-sessions-plan.adoc` ("Session entity").
//!
//! No `model.agent-session` spec section exists yet (an owner item the plan
//! itself names); this module cites only requirement ids that already exist
//! in `docs/spec/*.adoc` and leans on the same shapes those ids already
//! establish for [`crate::Issue`] and [`crate::review::Review`].

use facet::Facet;
use gix_hash::ObjectId;

use ents_model::MemberId;

/// Copy `oid` into the raw 20-byte form every oid-carrying field here
/// stores — `gix_hash::ObjectId` itself has no `Facet` impl, the same
/// reason [`ents_model::Redaction`] and [`ents_model::ResultRecord`] store
/// raw bytes behind an accessor rather than the type directly.
fn oid_bytes(oid: ObjectId) -> [u8; 20] {
    let mut bytes = [0u8; 20];
    bytes.copy_from_slice(oid.as_slice());
    bytes
}

/// The git blob hash of `bytes` — what `git hash-object` would report for
/// the identical content — computed without writing anything to a store.
/// [`AgentSession::plan_hash`] uses this so a confirm's binding is a content
/// hash of the plan text itself, independent of `facet-git-tree`'s own
/// `Option<String>` tree encoding for the `plan` field.
#[expect(
    clippy::expect_used,
    reason = "gix_object::compute_hash's Err variants concern streaming/writer failures that \
              cannot occur hashing a fixed, already-in-memory byte slice with a fixed hash kind"
)]
fn blob_hash(bytes: &[u8]) -> ObjectId {
    gix_object::compute_hash(gix_hash::Kind::Sha1, gix_object::Kind::Blob, bytes)
        .expect("hashing an in-memory byte slice cannot fail")
}

/// A session's resolved review policy: whether confirming it should
/// auto-open a review of the result (Phase 5) or leave that to the member.
/// A hard enum — like [`crate::review::Verdict`], not like
/// [`crate::Issue::state`] — because it gates a follow-on effect's
/// behavior rather than describing free-form domain data.
///
/// # Examples
///
/// ```
/// use ents_forge::agent::ReviewPolicy;
///
/// let policy: ReviewPolicy = "auto".parse().expect("known policy");
/// assert_eq!(policy, ReviewPolicy::Auto);
/// assert_eq!(policy.to_string(), "auto");
/// assert!("sometimes".parse::<ReviewPolicy>().is_err());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Facet)]
#[repr(u8)]
pub enum ReviewPolicy {
    /// Confirming the session's plan also opens a review automatically once
    /// a result lands (Phase 5).
    Auto,
    /// No review opens automatically; the member opens one manually.
    Manual,
}

impl std::str::FromStr for ReviewPolicy {
    type Err = crate::Error;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "auto" => Ok(Self::Auto),
            "manual" => Ok(Self::Manual),
            other => Err(crate::Error::InvalidArgument(format!(
                "unknown review policy {other:?}: expected auto or manual"
            ))),
        }
    }
}

impl std::fmt::Display for ReviewPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Auto => "auto",
            Self::Manual => "manual",
        })
    }
}

/// Why a session ended in [`Status::Failed`] — a struct rather than a bare
/// `String` so later phases can extend it additively (`model.extensibility`)
/// once Phase 2 teaches the effect runner to fill it in from a run's own
/// result taxonomy (`model.result-taxonomy`, owned by `ents-model`'s
/// `Status`, unrelated to this module's own [`Status`] despite the shared
/// name).
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct FailureReason {
    /// A human-readable account of what went wrong.
    pub detail: String,
}

/// A session's durable lifecycle phase. Only phases that persist between
/// commits are named here — `queued`, `awaiting confirmation`, and
/// `completing` are read off the tip snapshot instead
/// ([`AgentSession::queued`], [`AgentSession::awaiting_confirmation`]), per
/// the plan's own constraint: "Ephemeral boundaries ... are derived from the
/// session's commit chain and artifacts, never enumerated in the status
/// enum."
// @relation(model.extensibility, meta-ref.typed-tree, scope=file)
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
#[repr(u8)]
pub enum Status {
    /// No confirmed plan exists yet — either none has been drafted, or the
    /// member is redrafting one.
    Planning,
    /// A plan leaf exists. [`AgentSession::queued`] and
    /// [`AgentSession::awaiting_confirmation`] further distinguish whether
    /// it is bound by a current confirm.
    Ready,
    /// A worker has claimed the session and is executing it — the point of
    /// no return past which no plan revision or un-queue is legal.
    Running,
    /// The run completed and its result landed.
    Done,
    /// The run could not complete, or was refused, for the carried reason.
    Failed(FailureReason),
}

/// One toolchain the session's run depends on, hash-pinned to the
/// `refs/meta/toolchains/<name>` ref's tip at the moment the session was
/// created (`model.toolchain`) — so a later change to that toolchain never
/// retroactively alters what this session declared it needed.
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct ToolchainPin {
    /// The toolchain's own name (`refs/meta/toolchains/<name>`).
    pub name: String,
    oid: [u8; 20],
}

impl ToolchainPin {
    /// Pin `name` at its ref's current tip `oid`.
    #[must_use]
    pub fn new(name: impl Into<String>, oid: ObjectId) -> Self {
        Self {
            name: name.into(),
            oid: oid_bytes(oid),
        }
    }

    /// The pinned tip commit oid.
    #[must_use]
    pub fn oid(&self) -> ObjectId {
        ObjectId::from_bytes_or_panic(&self.oid)
    }
}

/// A session's typed metadata: everything about it that is not the plan
/// text, its confirmation, or its thread.
///
/// `member`, `created`, `started`, and `finished` duplicate what a full walk
/// of the session ref's own commit chain could in principle recover (the
/// genesis signer and commit times) — a deliberate departure from
/// [`crate::comment::Comment`]'s and [`ents_model::Member`]'s own
/// commit-chain-not-tree-field idiom (`meta-ref.identity-binding`), made
/// because a listing view (`docs/agent-sessions-plan.adoc`'s Phase 3) reads
/// many sessions at once and must not walk each one's full history just to
/// show who owns it and when it moved.
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct SessionMeta {
    /// The member who owns this session.
    pub member: MemberId,
    /// When the session was created, in seconds since the Unix epoch.
    pub created: i64,
    /// The worker that claimed this session, if one ever has
    /// ([`super::command::claim`] sets this) — the member whose signature
    /// the gate's designated-worker roster admits to advance this session
    /// on [`Self::member`]'s behalf (`docs/agent-sessions-plan.adoc`'s
    /// Phase 2a).
    pub worker: Option<MemberId>,
    /// The name of the sandbox executing this run, verbatim, once claimed
    /// ([`super::command::claim`] sets this) — the plan's own "sandbox
    /// name verbatim while running" (Phase 3's detail page).
    pub sprite: Option<String>,
    /// When a worker claimed the session and began running it, if it ever
    /// has ([`super::command::claim`] sets this).
    pub started: Option<i64>,
    /// When the run reached a terminal state, if it ever has
    /// ([`super::command::finish`] sets this).
    pub finished: Option<i64>,
    /// The model id the run executes against.
    pub model: String,
    /// The toolchains this run depends on, hash-pinned at creation.
    pub toolchains: Vec<ToolchainPin>,
    /// The session's durable lifecycle phase.
    pub status: Status,
    /// The ref the run executes against as its starting point.
    pub base_ref: String,
    /// The branch the worker pushes the run's commits to
    /// (`agent/<member>/<abbrev-genesis>`, per the plan's resolved-by-default
    /// item), unset until Phase 2's worker computes it — the session's own
    /// genesis oid does not exist yet at creation time to derive it from.
    pub result_branch: Option<String>,
    /// The review policy resolved for this session, overridable up to
    /// confirm time (Phase 5); [`super::Confirm::review_policy`] freezes
    /// whatever value was in force at confirm.
    pub review_policy: ReviewPolicy,
    retry_of: Option<[u8; 20]>,
}

impl SessionMeta {
    /// A new session's metadata at genesis: `status` is always
    /// [`Status::Planning`], and `started`/`finished`/`result_branch` are
    /// unset — Phase 2's effect worker fills them in as the run progresses.
    #[must_use]
    pub fn new(
        member: MemberId,
        created: i64,
        model: impl Into<String>,
        toolchains: Vec<ToolchainPin>,
        base_ref: impl Into<String>,
        review_policy: ReviewPolicy,
        retry_of: Option<ObjectId>,
    ) -> Self {
        Self {
            member,
            created,
            worker: None,
            sprite: None,
            started: None,
            finished: None,
            model: model.into(),
            toolchains,
            status: Status::Planning,
            base_ref: base_ref.into(),
            result_branch: None,
            review_policy,
            retry_of: retry_of.map(oid_bytes),
        }
    }

    /// The prior session this one retries, if any — the genesis oid of that
    /// session's own ref.
    #[must_use]
    pub fn retry_of(&self) -> Option<ObjectId> {
        self.retry_of
            .map(|bytes| ObjectId::from_bytes_or_panic(&bytes))
    }
}

/// A signed binding of a specific plan-leaf hash to a resolved review
/// policy — absent until a member approves the plan
/// [`AgentSession::plan`] currently carries. Confirm is a leaf, not a
/// status: whether the binding it carries still names the current plan is
/// read off the tip by [`AgentSession::queued`] and
/// [`AgentSession::awaiting_confirmation`], never stored as a boolean.
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct Confirm {
    plan_hash: [u8; 20],
    /// The review policy resolved at confirm time, frozen even if
    /// [`SessionMeta::review_policy`] changes afterward.
    pub review_policy: ReviewPolicy,
}

impl Confirm {
    /// Approve the plan whose content hash is `plan_hash`, under
    /// `review_policy`.
    #[must_use]
    pub fn new(plan_hash: ObjectId, review_policy: ReviewPolicy) -> Self {
        Self {
            plan_hash: oid_bytes(plan_hash),
            review_policy,
        }
    }

    /// The plan-leaf hash this confirm binds.
    #[must_use]
    pub fn plan_hash(&self) -> ObjectId {
        ObjectId::from_bytes_or_panic(&self.plan_hash)
    }
}

/// One agent session: a plan-and-confirm ceremony around a headless agent
/// run, its typed [`meta`](AgentSession::meta), its
/// [`plan`](AgentSession::plan) text, an optional
/// [`confirm`](AgentSession::confirm) binding that plan's hash, and a
/// `thread` of opaque, verbatim per-turn message blobs — never typed
/// internally, never rendered, redactable blob-by-blob
/// (`model.redaction`), exactly write-only audit material.
///
/// Identity is the oid of the session's own genesis commit
/// (`meta-ref.identity-binding`), the same sign-then-name shape
/// [`crate::Issue`] and [`crate::comment::Comment`] use — see
/// [`super::command::new`].
///
/// # Examples
///
/// ```
/// use ents_forge::agent::{AgentSession, ReviewPolicy, SessionMeta};
/// use ents_model::MemberId;
///
/// let session = AgentSession {
///     meta: SessionMeta::new(
///         MemberId::new("jdc"),
///         1_000,
///         "claude-sonnet-5",
///         vec![],
///         "refs/heads/main",
///         ReviewPolicy::Manual,
///         None,
///     ),
///     plan: None,
///     confirm: None,
///     thread: vec![b"start the task".to_vec()],
/// };
/// let (id, store) = facet_git_tree::serialize(&session).expect("serialize");
/// let back: AgentSession = facet_git_tree::deserialize(&id, &store).expect("deserialize");
/// assert_eq!(back, session);
/// assert!(!session.queued());
/// assert!(!session.awaiting_confirmation());
/// ```
// @relation(meta-ref.identity-binding, meta-ref.typed-tree, model.extensibility, model.redaction, scope=file)
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct AgentSession {
    /// The session's typed metadata.
    pub meta: SessionMeta,
    /// The plan text, or `None` before one has been drafted.
    pub plan: Option<String>,
    /// The current confirmation, or `None` before one has been recorded —
    /// also `None` again immediately after any plan revision
    /// ([`super::command::revise_plan`] drops it unconditionally).
    pub confirm: Option<Confirm>,
    /// Opaque, verbatim per-turn message blobs — write-only audit material,
    /// never decoded by this crate.
    pub thread: Vec<Vec<u8>>,
}

impl AgentSession {
    /// The current plan text's content hash (what `git hash-object` would
    /// report for it), or `None` when no plan has been drafted yet.
    #[must_use]
    pub fn plan_hash(&self) -> Option<ObjectId> {
        self.plan.as_deref().map(|text| blob_hash(text.as_bytes()))
    }

    /// _Queued_: `Ready` and the current confirm binds the current plan's
    /// hash — the plan is approved exactly as it now reads.
    #[must_use]
    pub fn queued(&self) -> bool {
        self.meta.status == Status::Ready
            && match (&self.confirm, self.plan_hash()) {
                (Some(confirm), Some(hash)) => confirm.plan_hash() == hash,
                _ => false,
            }
    }

    /// _Awaiting confirmation_: `Ready`, and not [`queued`](Self::queued) —
    /// the confirm leaf is absent, or it binds a plan hash the current plan
    /// text has since moved past.
    #[must_use]
    pub fn awaiting_confirmation(&self) -> bool {
        self.meta.status == Status::Ready && !self.queued()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "unit test")]

    use facet_git_tree::{deserialize, serialize};
    use rstest::rstest;

    use super::*;

    fn meta(status: Status) -> SessionMeta {
        let mut meta = SessionMeta::new(
            MemberId::new("jdc"),
            1_000,
            "claude-sonnet-5",
            vec![ToolchainPin::new(
                "rust-stable",
                ObjectId::from_bytes_or_panic(&[3u8; 20]),
            )],
            "refs/heads/main",
            ReviewPolicy::Manual,
            Some(ObjectId::from_bytes_or_panic(&[9u8; 20])),
        );
        meta.status = status;
        meta
    }

    fn session(status: Status, plan: Option<&str>, confirm: Option<Confirm>) -> AgentSession {
        AgentSession {
            meta: meta(status),
            plan: plan.map(str::to_owned),
            confirm,
            thread: vec![b"turn one".to_vec()],
        }
    }

    #[rstest]
    #[case::planning_no_plan(Status::Planning, None, None)]
    #[case::planning_with_plan(Status::Planning, Some("do the thing"), None)]
    #[case::ready_no_plan(Status::Ready, None, None)]
    #[case::ready_with_plan_no_confirm(Status::Ready, Some("do the thing"), None)]
    #[case::running(Status::Running, Some("do the thing"), None)]
    #[case::done(Status::Done, Some("do the thing"), None)]
    #[case::failed(
        Status::Failed(FailureReason { detail: "sandbox died".to_owned() }),
        Some("do the thing"),
        None
    )]
    // @relation(meta-ref.typed-tree, scope=function, role=Verifies)
    fn agent_session_round_trips_through_a_tree(
        #[case] status: Status,
        #[case] plan: Option<&str>,
        #[case] confirm: Option<Confirm>,
    ) {
        let session = session(status, plan, confirm);
        let (root, store) = serialize(&session).expect("serialize");
        let back: AgentSession = deserialize(&root, &store).expect("deserialize");
        assert_eq!(back, session);
    }

    // ---------------------------------------------------------------
    // Derived predicates: pure functions on the decoded tip.
    // ---------------------------------------------------------------

    /// Neither predicate holds outside `Ready`, confirm or no.
    #[rstest]
    #[case::planning(Status::Planning)]
    #[case::running(Status::Running)]
    #[case::done(Status::Done)]
    #[case::failed(Status::Failed(FailureReason { detail: "oops".to_owned() }))]
    // @relation(scope=function, role=Verifies)
    fn predicates_are_false_outside_ready(#[case] status: Status) {
        let plan = "do the thing";
        let confirming = Confirm::new(blob_hash(plan.as_bytes()), ReviewPolicy::Manual);
        let session = session(status, Some(plan), Some(confirming));
        assert!(!session.queued());
        assert!(!session.awaiting_confirmation());
    }

    /// `Ready` with no confirm at all is awaiting confirmation, never
    /// queued.
    #[rstest]
    // @relation(scope=function, role=Verifies)
    fn ready_with_no_confirm_is_awaiting_confirmation() {
        let session = session(Status::Ready, Some("do the thing"), None);
        assert!(session.awaiting_confirmation());
        assert!(!session.queued());
    }

    /// `Ready` with a confirm binding the exact current plan hash is
    /// queued, never awaiting confirmation.
    #[rstest]
    // @relation(scope=function, role=Verifies)
    fn ready_with_a_current_confirm_is_queued() {
        let plan = "do the thing";
        let confirm = Confirm::new(blob_hash(plan.as_bytes()), ReviewPolicy::Auto);
        let session = session(Status::Ready, Some(plan), Some(confirm));
        assert!(session.queued());
        assert!(!session.awaiting_confirmation());
    }

    /// Revising the plan text after a confirm was recorded makes the old
    /// confirm's binding stale: the session reverts to awaiting
    /// confirmation, never queued, purely as a function of the decoded tip
    /// — no separate "stale" flag exists anywhere.
    #[rstest]
    // @relation(scope=function, role=Verifies)
    fn a_plan_revision_makes_an_existing_confirm_stale() {
        let original = "do the thing";
        let confirm = Confirm::new(blob_hash(original.as_bytes()), ReviewPolicy::Manual);
        let mut session = session(Status::Ready, Some(original), Some(confirm));
        assert!(session.queued());

        session.plan = Some("do the other thing instead".to_owned());
        assert!(
            !session.queued(),
            "a confirm bound to the old plan hash must not read as queued against new text"
        );
        assert!(session.awaiting_confirmation());
    }

    /// A confirm binding an absent plan (impossible through the command
    /// layer, but not through this predicate) never reads as queued.
    #[rstest]
    // @relation(scope=function, role=Verifies)
    fn a_confirm_cannot_queue_an_absent_plan() {
        let confirm = Confirm::new(blob_hash(b"some prior plan"), ReviewPolicy::Manual);
        let session = session(Status::Ready, None, Some(confirm));
        assert!(!session.queued());
        assert!(session.awaiting_confirmation());
    }

    #[rstest]
    #[case::auto(ReviewPolicy::Auto)]
    #[case::manual(ReviewPolicy::Manual)]
    // @relation(meta-ref.typed-tree, scope=function, role=Verifies)
    fn review_policy_round_trips(#[case] policy: ReviewPolicy) {
        let (id, store) = serialize(&policy).expect("serialize");
        let back: ReviewPolicy = deserialize(&id, &store).expect("deserialize");
        assert_eq!(back, policy);
    }

    #[rstest]
    // @relation(scope=function, role=Verifies)
    fn review_policy_parses_its_own_display_form() {
        for policy in [ReviewPolicy::Auto, ReviewPolicy::Manual] {
            let parsed: ReviewPolicy = policy.to_string().parse().expect("round trips");
            assert_eq!(parsed, policy);
        }
    }

    #[rstest]
    // @relation(scope=function, role=Verifies)
    fn retry_of_round_trips_through_the_accessor() {
        let target = ObjectId::from_bytes_or_panic(&[5u8; 20]);
        let meta = SessionMeta::new(
            MemberId::new("jdc"),
            1_000,
            "claude-sonnet-5",
            vec![],
            "refs/heads/main",
            ReviewPolicy::Auto,
            Some(target),
        );
        assert_eq!(meta.retry_of(), Some(target));
    }
}
