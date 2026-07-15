//! Phase 5 — durability ordering (`verify/exercise.md`, "Phase 5"),
//! replacing the deleted `verify/tla/Durability.tla`.
//!
//! SKELETON. Every action body is `todo!()`; filling in
//! [`Model::actions`] and [`Model::next_state`] for real is the human
//! exercise.
//!
//! Deployment note: the exercise document frames this phase around a
//! hosted Tigris-object-store + Postgres-CAS split. The project
//! currently deploys neither — serving is plain git http-backend over
//! one filesystem — so the state and actions here are named for the
//! general shape (object write, ref CAS, crash), and the Tigris/Pg
//! instantiation is deferred until such a deployment exists. The
//! invariant under study is unchanged: no ref points outside the
//! durable object set.

#![expect(clippy::todo, reason = "Phase 5 skeleton — filling this in is the human exercise, not this scaffold's job")]

use stateright::{Model, Property};

/// Durability state: which objects have reached durable storage, and
/// the ref store's current tips. SKELETON.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct State {
    /// Objects durably written so far.
    pub durable: Vec<&'static str>,
    /// Current tip of each ref in `crate::REFS`, in the same order;
    /// `None` means the ref is unborn.
    pub refs: [Option<&'static str>; 4],
}

/// Durability actions (`verify/exercise.md`, Phase 5): `ObjectWrite`,
/// `RefCAS`, `Crash` at any point. SKELETON.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    /// An object (or pack) reaches durable storage.
    ObjectWrite,
    /// The ref store compare-and-swaps a tip — the write-order question
    /// this phase exists to settle lives here.
    RefCas,
    /// Crash at any point; recovery obligations follow from what
    /// survives.
    Crash,
}

/// The Phase 5 durability model. SKELETON.
pub struct DurabilityModel;

impl Model for DurabilityModel {
    type State = State;
    type Action = Action;

    fn init_states(&self) -> Vec<Self::State> {
        vec![State::default()]
    }

    fn actions(&self, _state: &Self::State, _actions: &mut Vec<Self::Action>) {
        todo!("exercise: enumerate ObjectWrite/RefCas/Crash per verify/exercise.md Phase 5")
    }

    fn next_state(&self, _last_state: &Self::State, _action: Self::Action) -> Option<Self::State> {
        todo!("exercise: Phase 5's transition relation, including crash faults")
    }

    fn properties(&self) -> Vec<Property<Self>> {
        vec![
            // The invariant this phase exists to prove: no ref in the
            // ref store points outside the durable object set. STATED
            // here so the ledger row has a formal object to point at;
            // NOT proved — the transition relation above is a stub, so
            // checking this today says nothing.
            Property::always("refs_point_durable", |_, _| true /* TODO(exercise) */),
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
    #[ignore = "exercise stub: Phase 5's transition relation is unwritten"]
    fn model_runs() {
        let _ = DurabilityModel.checker().spawn_bfs().join();
    }
}
