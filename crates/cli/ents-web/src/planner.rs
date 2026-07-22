//! The planning-chat page's LLM seam (`docs/agent-sessions-plan.adoc`'s
//! Phase 4, "Plan-mode Agent SDK with the member's credential"): a small
//! trait an assistant turn is generated through, injected via
//! [`crate::state::AppState`] exactly like
//! [`crate::identity::SigningIdentity`] is (`roots.web-agnostic`) — a
//! composition root wires whichever implementation its deployment can
//! offer; nothing in [`crate::pages`] loads a credential or calls out to a
//! model directly.
//!
//! Per-member credentials (BYOK) are `docs/agent-sessions-plan.adoc`'s
//! Phase 6, explicitly out of scope here — this crate ships only
//! [`UnconfiguredPlanner`], the default every composition root installs
//! until a real one exists. A future real implementation lives outside
//! this crate (wherever a member's credential is resolved) and is injected
//! the same way [`crate::identity::SigningIdentity`]'s real
//! implementations are.

use ents_forge::agent::AgentSession;

/// Produce the assistant's next reply inside a session's ongoing planning
/// conversation.
///
/// Read-only by design: a [`Planner`] never mutates a session itself — the
/// planning-chat page's own handler is what appends the resulting turns to
/// `thread` (`ents_forge::agent::append_thread`) and, separately, commits
/// any drafted plan text the member asks to keep
/// (`ents_forge::agent::revise_plan`).
///
/// # Examples
///
/// ```
/// use ents_forge::agent::{AgentSession, ReviewPolicy, SessionMeta};
/// use ents_model::MemberId;
/// use ents_web::planner::{Planner, UnconfiguredPlanner};
///
/// let session = AgentSession {
///     meta: SessionMeta::new(
///         MemberId::new("jdc"), 1_000, "claude-sonnet-5", vec![],
///         "refs/heads/main", ReviewPolicy::Manual, None,
///     ),
///     plan: None,
///     confirm: None,
///     thread: vec![b"fix the flaky test".to_vec()],
/// };
/// let planner = UnconfiguredPlanner;
/// assert!(!planner.reply(&session, "what's the plan?").is_empty());
/// ```
pub trait Planner: Send + Sync {
    /// Reply to `message`, given `session`'s current state (its seeded
    /// prompt, prior thread turns, and any plan already drafted) for
    /// context.
    fn reply(&self, session: &AgentSession, message: &str) -> String;
}

/// The default composition root's [`Planner`]: per-member credentials are
/// not wired in this build, so this renders one fixed, honest notice
/// instead of ever calling out to a real model — the chat page renders its
/// reply exactly like any other assistant turn, with no special-casing for
/// "no backend" beyond the text itself.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnconfiguredPlanner;

impl Planner for UnconfiguredPlanner {
    fn reply(&self, _session: &AgentSession, _message: &str) -> String {
        "Planning backend not configured for this deployment (per-member credentials are \
         Phase 6's own scope, not yet wired). Draft the plan text yourself below and commit it \
         with \u{201c}Commit plan\u{201d}, or run `git ents agent plan` from the command line."
            .to_owned()
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn session() -> AgentSession {
        AgentSession {
            meta: ents_forge::agent::SessionMeta::new(
                ents_model::MemberId::new("jdc"),
                1_000,
                "claude-sonnet-5",
                vec![],
                "refs/heads/main",
                ents_forge::agent::ReviewPolicy::Manual,
                None,
            ),
            plan: None,
            confirm: None,
            thread: vec![b"fix the flaky test".to_vec()],
        }
    }

    #[rstest]
    // @relation(scope=function, role=Verifies)
    fn unconfigured_planner_replies_with_a_fixed_notice_regardless_of_input() {
        let planner = UnconfiguredPlanner;
        let first = planner.reply(&session(), "what's the plan?");
        let second = planner.reply(&session(), "a completely different message");
        assert_eq!(first, second);
        assert!(first.to_lowercase().contains("not configured"));
    }
}
