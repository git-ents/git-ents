//! The agent sub-domain: the [`AgentSession`] entity (`entity`), the
//! `agent` command's business logic (`command`), and the `agent` subcommand's
//! argument grammar (`cli`) — the same three-file split [`crate::issue`] and
//! [`crate::review`] use, for the same reason: the data shape, the command
//! mechanism, and the CLI grammar stay easy to read independently.
//!
//! Phase 1 of `docs/agent-sessions-plan.adoc` ("Session entity"); Phase 1b
//! (lifecycle invariants in `ents-gate-rules`) and everything after it are
//! out of scope here.

mod cli;
mod command;
mod dispatch;
mod entity;

pub use cli::AgentAction;
pub use command::{
    ClaimAgentSession, FinishAgentSession, FinishOutcome, NewAgentSession, append_thread, claim,
    confirm, draft_plan, draft_plan_transition, finish, finish_transition, list, list_all, new,
    reopen, revise_plan, show,
};
pub use dispatch::{
    Dispatch, PlanDispatch, ReviewDispatch, dispatch, dispatch_plan, dispatch_review,
};
pub use entity::{
    AgentSession, Confirm, FailureReason, ReviewPolicy, SessionMeta, Status, ToolchainPin,
};
