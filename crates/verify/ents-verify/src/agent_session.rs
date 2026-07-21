//! The agent-session lifecycle model (`docs/agent-sessions-plan.adoc`
//! Phase 1b), a [`stateright`] model over the derived-predicate lifecycle
//! Phase 1's `ents_forge::agent` module establishes: `planning ⇄ ready →
//! running → done | failed`, confirm as a plan-hash binding rather than a
//! boolean, and `queued`/`awaiting confirmation` read off the tip snapshot.
//!
//! Unlike [`crate::receive`], [`crate::effects`], and [`crate::durability`]
//! (Phase 3–5 skeletons, deliberately `todo!()` — filling them in is the
//! human exercise), this model is filled in completely: Phase 1b's own
//! acceptance criteria ask for a working model of the lifecycle, mirroring
//! [`crate::search`]'s fully-built exhaustive search rather than a stub.
//!
//! # The bounded universe
//!
//! Two plan generations ([`PlanGen::A`], [`PlanGen::B`]) are enough to
//! exercise every transition the lifecycle names, including the one the
//! entity's own doc calls out by name: drafting `B` after confirming `A`
//! invalidates the existing confirm even though nothing about `B` reuses
//! `A`'s hash for anything (`ents_forge::agent::command::revise_plan`'s
//! "never compares the new text's hash against the old confirm's before
//! dropping it"). A third generation would grow the reachable state count
//! without adding a new transition shape, the same scope discipline
//! `crate::search`'s module doc explains for its own bound.

use stateright::{Model, Property};

/// A plan's content-hash identity, standing in for `AgentSession::plan_hash`'s
/// real git blob hash — two distinct values are enough to exercise every
/// transition (see the module's bounded-universe note).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PlanGen {
    /// The first plan drafted.
    A,
    /// A later revision.
    B,
}

/// Both plan generations in the bounded universe, in a fixed enumeration
/// order.
pub const PLAN_GENS: [PlanGen; 2] = [PlanGen::A, PlanGen::B];

/// The durable lifecycle phase (`ents_forge::agent::Status`), minus
/// `Status::Failed`'s carried `FailureReason` detail — irrelevant to the
/// invariants this model checks and would only inflate the state space.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Status {
    /// No confirmed plan exists yet.
    #[default]
    Planning,
    /// A plan leaf exists; [`State::queued`] further distinguishes whether
    /// it is bound by a current confirm.
    Ready,
    /// A worker has claimed the session — the point of no return.
    Running,
    /// The run completed and its result landed.
    Done,
    /// The run could not complete, or was refused.
    Failed,
}

/// One session's modeled tip: durable status, the current plan generation
/// (`None` before a plan is ever drafted), and the generation a confirm
/// leaf binds (`None` when absent). Mirrors `AgentSession`'s three
/// tip-relevant fields exactly — `thread` and the rest of `SessionMeta`
/// play no part in the lifecycle invariants this model checks.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct State {
    /// The session's durable lifecycle phase.
    pub status: Status,
    /// The current plan's generation, or `None` before one is drafted.
    pub plan: Option<PlanGen>,
    /// The generation the current confirm leaf binds, or `None` when
    /// absent.
    pub confirm: Option<PlanGen>,
}

impl State {
    /// `AgentSession::queued`: `Ready`, and the confirm binds the current
    /// plan's generation exactly — restated over this model's [`State`]
    /// rather than a decoded tree.
    #[must_use]
    pub fn queued(&self) -> bool {
        self.status == Status::Ready && self.confirm.is_some() && self.confirm == self.plan
    }
}

/// Session-lifecycle actions (`docs/agent-sessions-plan.adoc` Phase 1b):
/// draft or revise the plan, confirm it, un-queue (drop confirm, the
/// plan's resolved-by-default item 1), claim (the worker's point of no
/// return), and finish.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    /// `git ents agent plan`: draft or redraft the plan to the given
    /// generation, unconditionally dropping any existing confirm
    /// (`ents_forge::agent::command::revise_plan`) — offered even when the
    /// plan already carries this generation, mirroring that command's own
    /// "a plan revision that happens to land on byte-identical text is a
    /// degenerate case not worth special-casing".
    DraftPlan(PlanGen),
    /// `git ents agent confirm`: bind the confirm leaf to the current
    /// plan's generation.
    Confirm,
    /// Un-queue (resolved-by-default item 1 of `docs/agent-sessions-plan.
    /// adoc`): drop the confirm leaf, returning the session to `Planning`
    /// — legal only before claim.
    UnQueue,
    /// The worker's claim: legal only when `Ready` and [`State::queued`].
    Claim,
    /// The run reaches a terminal state.
    Finish(Terminal),
}

/// The two terminal outcomes a run can finish in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Terminal {
    /// The run completed and its result landed.
    Done,
    /// The run could not complete, or was refused.
    Failed,
}

/// The Phase 1b lifecycle model.
pub struct AgentSessionModel;

impl Model for AgentSessionModel {
    type State = State;
    type Action = Action;

    fn init_states(&self) -> Vec<Self::State> {
        vec![State::default()]
    }

    fn actions(&self, state: &Self::State, actions: &mut Vec<Self::Action>) {
        match state.status {
            Status::Planning | Status::Ready => {
                actions.extend(PLAN_GENS.iter().map(|g| Action::DraftPlan(*g)));
                if state.status == Status::Ready && state.plan.is_some() {
                    actions.push(Action::Confirm);
                }
                if state.status == Status::Ready && state.confirm.is_some() {
                    actions.push(Action::UnQueue);
                }
                if state.queued() {
                    actions.push(Action::Claim);
                }
            }
            Status::Running => {
                actions.push(Action::Finish(Terminal::Done));
                actions.push(Action::Finish(Terminal::Failed));
            }
            // Terminal states absorb: no action ever leaves them.
            Status::Done | Status::Failed => {}
        }
    }

    fn next_state(&self, last_state: &Self::State, action: Self::Action) -> Option<Self::State> {
        let mut state = last_state.clone();
        match action {
            Action::DraftPlan(plan_gen)
                if matches!(state.status, Status::Planning | Status::Ready) =>
            {
                state.plan = Some(plan_gen);
                state.confirm = None;
                state.status = Status::Ready;
            }
            Action::Confirm if state.status == Status::Ready && state.plan.is_some() => {
                state.confirm = state.plan;
            }
            Action::UnQueue if state.status == Status::Ready && state.confirm.is_some() => {
                state.confirm = None;
                state.status = Status::Planning;
            }
            Action::Claim if state.queued() => {
                state.status = Status::Running;
            }
            Action::Finish(terminal) if state.status == Status::Running => {
                state.status = match terminal {
                    Terminal::Done => Status::Done,
                    Terminal::Failed => Status::Failed,
                };
            }
            _ => return None,
        }
        Some(state)
    }

    fn properties(&self) -> Vec<Property<Self>> {
        vec![
            // "Running is unreachable without a confirm binding the
            // then-current plan": since neither Claim nor Finish touch
            // `plan`/`confirm`, and Claim's own guard is `queued()`, this
            // holds throughout every state Running is ever observed in,
            // not only at the instant of the claim.
            Property::always("running_requires_confirmed_plan", |_, s: &State| {
                s.status != Status::Running || (s.confirm.is_some() && s.confirm == s.plan)
            }),
            // "Plan revision invalidates prior confirm": no reachable
            // state ever carries a confirm bound to a generation other
            // than the current plan — `DraftPlan` drops it
            // unconditionally, so a stale binding never survives a
            // revision.
            Property::always("confirm_never_binds_a_stale_plan", |_, s: &State| {
                s.confirm.is_none() || s.confirm == s.plan
            }),
            // "Terminal states absorb": `Done` and `Failed` offer no
            // action at all.
            Property::always("terminal_states_absorb", |model, s: &State| {
                if matches!(s.status, Status::Done | Status::Failed) {
                    let mut actions = Vec::new();
                    model.actions(s, &mut actions);
                    actions.is_empty()
                } else {
                    true
                }
            }),
        ]
    }
}

#[cfg(test)]
mod tests {
    use stateright::Checker;

    use super::*;

    /// The lifecycle model, run to exhaustion, satisfies every property
    /// this module states — the properties are believed sound, unlike
    /// `crate::search`'s one deliberately-open ledger gap.
    #[test]
    fn lifecycle_satisfies_every_invariant() {
        let checker = AgentSessionModel.checker().spawn_bfs().join();
        checker.assert_no_discovery("running_requires_confirmed_plan");
        checker.assert_no_discovery("confirm_never_binds_a_stale_plan");
        checker.assert_no_discovery("terminal_states_absorb");
    }

    /// A deliberately broken variant of [`AgentSessionModel`]: `Claim` is
    /// enabled whenever the session is merely `Ready`, without requiring
    /// [`State::queued`]. Proves `running_requires_confirmed_plan` has
    /// teeth — remove the real guard and the checker finds the witness.
    struct ClaimWithoutConfirmGuard;

    impl Model for ClaimWithoutConfirmGuard {
        type State = State;
        type Action = Action;

        fn init_states(&self) -> Vec<Self::State> {
            AgentSessionModel.init_states()
        }

        fn actions(&self, state: &Self::State, actions: &mut Vec<Self::Action>) {
            AgentSessionModel.actions(state, actions);
            if state.status == Status::Ready && !state.queued() {
                actions.push(Action::Claim);
            }
        }

        fn next_state(
            &self,
            last_state: &Self::State,
            action: Self::Action,
        ) -> Option<Self::State> {
            if action == Action::Claim && last_state.status == Status::Ready {
                let mut state = last_state.clone();
                state.status = Status::Running;
                return Some(state);
            }
            AgentSessionModel.next_state(last_state, action)
        }

        fn properties(&self) -> Vec<Property<Self>> {
            vec![Property::always(
                "running_requires_confirmed_plan",
                |_, s: &State| {
                    s.status != Status::Running || (s.confirm.is_some() && s.confirm == s.plan)
                },
            )]
        }
    }

    #[test]
    fn the_claim_confirm_guard_is_load_bearing() {
        let checker = ClaimWithoutConfirmGuard.checker().spawn_bfs().join();
        let path = checker.assert_any_discovery("running_requires_confirmed_plan");
        println!(
            "running_requires_confirmed_plan witness (unguarded claim): {:?}",
            path.into_actions()
        );
    }

    /// A second deliberately broken variant: `DraftPlan` no longer drops
    /// the existing confirm. Proves `confirm_never_binds_a_stale_plan` has
    /// teeth — the real `DraftPlan` always drops confirm unconditionally
    /// (`ents_forge::agent::command::revise_plan`'s own contract); remove
    /// that and the checker finds a state where a confirm outlives the
    /// plan hash it bound.
    struct ReviseWithoutDroppingConfirm;

    impl Model for ReviseWithoutDroppingConfirm {
        type State = State;
        type Action = Action;

        fn init_states(&self) -> Vec<Self::State> {
            AgentSessionModel.init_states()
        }

        fn actions(&self, state: &Self::State, actions: &mut Vec<Self::Action>) {
            AgentSessionModel.actions(state, actions);
        }

        fn next_state(
            &self,
            last_state: &Self::State,
            action: Self::Action,
        ) -> Option<Self::State> {
            if let Action::DraftPlan(plan_gen) = action
                && matches!(last_state.status, Status::Planning | Status::Ready)
            {
                let mut state = last_state.clone();
                state.plan = Some(plan_gen);
                // Bug: the confirm leaf is left untouched.
                state.status = Status::Ready;
                return Some(state);
            }
            AgentSessionModel.next_state(last_state, action)
        }

        fn properties(&self) -> Vec<Property<Self>> {
            vec![Property::always(
                "confirm_never_binds_a_stale_plan",
                |_, s: &State| s.confirm.is_none() || s.confirm == s.plan,
            )]
        }
    }

    #[test]
    fn the_revise_drops_confirm_guard_is_load_bearing() {
        let checker = ReviseWithoutDroppingConfirm.checker().spawn_bfs().join();
        let path = checker.assert_any_discovery("confirm_never_binds_a_stale_plan");
        println!(
            "confirm_never_binds_a_stale_plan witness (revise without dropping confirm): {:?}",
            path.into_actions()
        );
    }
}
