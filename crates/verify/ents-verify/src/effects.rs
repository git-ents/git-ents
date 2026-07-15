//! Phase 4 — effects (`verify/exercise.md`, "Phase 4"), replacing the
//! deleted `verify/tla/Effects.tla`, building on [`crate::receive`]'s
//! state.
//!
//! SKELETON. Every action body is `todo!()`; filling in
//! [`Model::actions`] and [`Model::next_state`] for real is the human
//! exercise.
//!
//! Discharges, once filled in: `docs/abstractions.adoc` §6 (monotone,
//! exactly-once effects); `docs/spec/effect.adoc` (trigger set, dedup
//! key `(effect, refname, new_oid)`, results write-back, admin-only
//! authoring). Note `ents_gate_rules`' module docs mark the
//! cross-transaction dedup obligation as a deliberate gap — this model
//! is where that obligation lives.

#![expect(clippy::todo, reason = "Phase 4 skeleton — filling this in is the human exercise, not this scaffold's job")]

use stateright::{Model, Property};

/// Protocol state, extending [`crate::receive::State`] with effect
/// bookkeeping: which commits have entered the trigger set (§6), the
/// at-least-once delivery queue's dedup keys, and the result-ref writes
/// performed so far. SKELETON.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct State {
    /// The Phase 3 protocol state this phase builds on.
    pub receive: crate::receive::State,
    /// Commits that have entered the trigger set (§6: "fires once per
    /// commit that enters the set").
    pub triggered: Vec<&'static str>,
    /// At-least-once delivery: dedup keys `(effect, refname, new_oid)`
    /// currently enqueued.
    pub queue: Vec<(&'static str, &'static str, &'static str)>,
    /// Result-ref writes performed so far, keyed the same way as
    /// [`Self::queue`].
    pub results: Vec<(&'static str, &'static str, &'static str)>,
}

/// Protocol actions (`verify/exercise.md`, Phase 4): `RefAdvance`,
/// `TriggerEval`, `Enqueue`, `Execute`, `ResultPush`. SKELETON.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    /// A gated ref advance (reuses [`crate::receive::gate_admits`] when
    /// composed with Phase 3).
    RefAdvance,
    /// A commit enters the trigger set (§6). The delete-and-repush
    /// re-entry question — is triggering monotone? — lives here.
    TriggerEval,
    /// At-least-once enqueue of a dedup key.
    Enqueue,
    /// The executor runs an effect; may crash and restart (duplicate
    /// delivery).
    Execute,
    /// Result write-back: a gated write like any other, by an executor
    /// member key (`effect.results-writeback`).
    ResultPush,
}

/// The Phase 4 effects model. SKELETON.
pub struct EffectsModel;

impl Model for EffectsModel {
    type State = State;
    type Action = Action;

    fn init_states(&self) -> Vec<Self::State> {
        vec![State::default()]
    }

    fn actions(&self, _state: &Self::State, _actions: &mut Vec<Self::Action>) {
        todo!("exercise: enumerate RefAdvance/TriggerEval/Enqueue/Execute/ResultPush per verify/exercise.md Phase 4")
    }

    fn next_state(&self, _last_state: &Self::State, _action: Self::Action) -> Option<Self::State> {
        todo!("exercise: Phase 4's transition relation")
    }

    fn properties(&self) -> Vec<Property<Self>> {
        vec![
            // Obligation 1: the dedup key (effect, refname, new_oid)
            // yields result-ref idempotency under duplicate delivery
            // and executor crash-restart.
            Property::always("exactly_once_observable_effect", |_, _| true /* TODO(exercise) */),
            // Obligation 2: a ref deleted and re-pushed to the same oid
            // — does the commit re-enter the trigger set? Defines
            // whether triggers are monotone.
            Property::always("trigger_set_monotone", |_, _| true /* TODO(exercise) */),
            // Obligation 3: no sequence lets a non-admin cause execution
            // of content they authored as an effect (composes with
            // crate::search's binding_refname_recomputed property —
            // this was the original cross-ref replay scenario).
            Property::always("authorization_asymmetry", |_, _| true /* TODO(exercise) */),
        ]
    }
}

#[cfg(test)]
mod tests {
    use stateright::Checker;

    use super::*;

    /// Running this model requires [`Model::actions`] and
    /// [`Model::next_state`], both `todo!()` until the human exercise
    /// fills them in. Ignored so `cargo test --workspace` stays green
    /// while the skeleton exists.
    #[test]
    #[ignore = "exercise stub: Phase 4's transition relation is unwritten"]
    fn model_runs() {
        let _ = EffectsModel.checker().spawn_bfs().join();
    }
}
