//! Phase 3 — gate and receive as a protocol (`verify/exercise.md`,
//! "Phase 3"), replacing the deleted `verify/tla/Receive.tla`.
//!
//! SKELETON, with one deliberate exception: [`gate_admits`] calls
//! [`ents_gate_rules::gate`] directly on a transaction's `Facts` — that
//! direct call *is* the refinement mapping this module's `GateCheck`
//! action would enable on, replacing what `Receive.tla`'s `GateAdmits`
//! transcribed by hand. Every other action body is `todo!()`; filling in
//! [`Model::actions`] and [`Model::next_state`] for real is the human
//! exercise, not this scaffold's job.
//!
//! Discharges, once filled in: `docs/abstractions.adoc` §5 (tip
//! invariant, adoption, revocation), §4 (anti-replay);
//! `docs/spec/receive.adoc`; `docs/spec/gate.adoc` epoch bootstrap.

#![expect(clippy::todo, reason = "Phase 3 skeleton — filling this in is the human exercise, not this scaffold's job")]

use ents_gate_rules::{Facts, gate};
use stateright::{Model, Property};

/// `gate(facts) = {}`: the enabling condition a filled-in `GateCheck`
/// action would use. This is real code, not a stub — it is the seven
/// denial rules, called directly, standing in for `Receive.tla`'s
/// hand-transcribed `GateAdmits`.
#[must_use]
pub fn gate_admits(facts: Facts) -> bool {
    gate(facts).is_empty()
    // TODO(exercise): refname recomputation from signed content (the §4
    // binding rule ents-gate-rules omits; ledger row DIVERGED,
    // crate::search::SearchModel's `binding_refname_recomputed`
    // property) and the epoch rule (§5) are not part of `gate()`, so
    // this enabling condition inherits both gaps. Phase 3 decides
    // whether `ents-gate` proper's check composes in here.
}

/// Protocol state (§5): the current tip of every ref in the bounded
/// universe (`crate::REFS`), the object set, enrolled members, and the
/// config ref's epoch. SKELETON — named for the exercise's obligations,
/// not yet wired to a transition relation.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct State {
    /// Current tip of each ref in `crate::REFS`, in the same order;
    /// `None` means the ref is unborn.
    pub refs: [Option<&'static str>; 4],
    /// Objects the store currently has.
    pub objects: Vec<&'static str>,
    /// Enrolled member keys.
    pub members: Vec<&'static str>,
    /// The config ref's epoch (§5: "the epoch-setting commit is the
    /// first gated tip of the config ref").
    pub epoch: u32,
}

/// Protocol actions (`verify/exercise.md`, Phase 3): `Propose`,
/// `GateCheck`, `CAS`, `AdoptMerge`, `SelfMerge`. SKELETON.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    /// A writer proposes a transaction (two writers minimum, per the
    /// exercise's model).
    Propose,
    /// The gate evaluates a proposed transaction; a filled-in
    /// implementation enables this action exactly when [`gate_admits`]
    /// holds for the proposal in play.
    GateCheck,
    /// Compare-and-swap the ref tip — the anti-replay mechanism §4
    /// relies on parent-hash freshness for.
    Cas,
    /// Adoption: contributor commit in ancestry, adopter signature at
    /// tip, always a merge (never a rewrite).
    AdoptMerge,
    /// Two of one member's own machines racing a single-writer ref —
    /// `docs/abstractions.adoc` §4's same-actor divergence.
    SelfMerge,
}

/// The Phase 3 protocol model. SKELETON.
pub struct ReceiveModel;

impl Model for ReceiveModel {
    type State = State;
    type Action = Action;

    fn init_states(&self) -> Vec<Self::State> {
        vec![State::default()]
    }

    fn actions(&self, _state: &Self::State, _actions: &mut Vec<Self::Action>) {
        todo!("exercise: enumerate Propose/GateCheck/Cas/AdoptMerge/SelfMerge per verify/exercise.md Phase 3")
    }

    fn next_state(&self, _last_state: &Self::State, _action: Self::Action) -> Option<Self::State> {
        todo!("exercise: Phase 3's transition relation, using gate_admits as GateCheck's enabling condition")
    }

    fn properties(&self) -> Vec<Property<Self>> {
        vec![
            // Obligation 1: "the tip of a meta-ref is signed by a
            // member authorized for that refname" is preserved by every
            // action — pay attention to SelfMerge (is the merge commit
            // itself signed in the implementation? see ents-sync/src
            // and ents-receive/src/reconcile.rs).
            Property::always("tip_invariant_inductive", |_, _| true /* TODO(exercise) */),
            // Obligation 2: adoption preserves the tip invariant, even
            // when the contributor's commit is itself a merge of
            // unauthorized commits.
            Property::always("adoption_preserves_tip_invariant", |_, _| true /* TODO(exercise) */),
            // Obligation 3: a replayed genesis against a not-yet-created
            // ref is safe only in conjunction with Phase 2's binding
            // totality — state the exact conjunction.
            Property::always("anti_replay", |_, _| true /* TODO(exercise) */),
            // Obligation 4: from an empty store, is there a state where
            // the gate must read the epoch from a ref whose tip is not
            // yet gated? Cite ents-gate/src/{config,policy}.rs.
            Property::always("epoch_bootstrap", |_, _| true /* TODO(exercise) */),
            // Obligation 5: a member valid at admission and revoked
            // later — does naive re-verification of historical tips
            // fail, and does the epoch mechanism actually prevent that?
            Property::always("revocation", |_, _| true /* TODO(exercise) */),
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
    /// while the skeleton exists — the one permitted use of `#[ignore]`
    /// in this codebase, because this is a declared stub, not a passed-
    /// off result.
    #[test]
    #[ignore = "exercise stub: Phase 3's transition relation is unwritten"]
    fn model_runs() {
        let _ = ReceiveModel.checker().spawn_bfs().join();
    }
}
