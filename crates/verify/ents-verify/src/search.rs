//! Phase 0.5 — verify the verifier (`verify/exercise.md`, "Phase 0.5"),
//! replacing `verify/alloy/gate_rules.als`'s `check` commands with an
//! exhaustive [`stateright`] search over the crate's own vocabulary.
//!
//! `ents_gate_rules` can *evaluate* its seven denial rules over one
//! supplied transaction; it cannot search for the transaction nobody
//! thought of. This module runs that search: [`SearchModel`]'s states
//! are a transaction under construction, one choice at a time, over the
//! bounded universe in `crate::{REFS, OIDS, KEYS}` plus the shadow
//! [`Kind`](crate::Kind) annotation, and its six [`Property::always`]
//! obligations restate — independently, in hand-written Rust, not by
//! calling `gate` a second time — exactly the five doc invariants the
//! seven rules claim to cover, plus the refname-binding claim they do
//! not. A property's *discovery* is a transaction `gate` admits that
//! violates the corresponding independent check.
//!
//! # The bound, and why it stays this small
//!
//! A state is exactly five choices, made in a fixed order: refname,
//! transaction [`Shape`], [`Signer`], [`Retention`], and the new tip's
//! [`Kind`]. That is `4 x 4 x 3 x 4 x 3 = 576` leaf states — small enough
//! for [`Model::checker`] to explore exhaustively in well under a
//! second. A naive "add any single EDB atom from the domain" search (a
//! literal transcription of Alloy's relational style) was tried first
//! and rejected: with `parent`/`signed_by`/`anchor`/`context`/
//! `object_exists` each ranging freely over `OIDS x OIDS` or `OIDS x
//! KEYS`, the reachable state count is the size of the powerset of every
//! possible atom, not the five-transaction-shapes count above — tens of
//! thousands of atoms' worth of subsets, intractable for exhaustive BFS.
//! [`Shape`] instead enumerates the four transaction shapes
//! `ents_gate_rules`' own unit tests already hand-build (creation,
//! fast-forward advance, non-fast-forward, and the second-root merge),
//! which is everything the seven rules' *logic* actually branches on;
//! membership is fixed background ([`crate::enroll_all`]) rather than a
//! search dimension, since membership *lifecycle* is Phase 3's concern
//! (`receive.rs`), not Phase 0.5's.

use ents_gate_rules::{Facts, gate};
use stateright::{Model, Property};

use crate::{ADMIN_KEY, Kind, MEMBER_KEY_1, REFS, enroll_all};

/// The shape of the proposed transaction — the one dimension along which
/// the seven denial rules' *logic* actually branches, standing in for
/// the full `parent`/`ref_update` relations' combinatorics.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Shape {
    /// `ref_update(r, None, new)` with `new` parentless: entity creation.
    Genesis,
    /// `ref_update(r, Some(old), new)` with `new` a descendant of `old`
    /// via one intermediate commit — the admitted case.
    FastForward,
    /// `ref_update(r, Some(old), new)` with `new` unrelated to `old` —
    /// `ff_violation`'s witness.
    NonFf,
    /// `ref_update(r, Some(old), new)` with `new` a merge of the
    /// fast-forward chain and an unrelated parentless commit —
    /// `second_root_violation`'s witness
    /// (`ents_gate_rules::tests::merged_in_second_root_is_rejected`).
    SecondRoot,
}

/// All four transaction shapes, in a fixed enumeration order.
pub const SHAPES: [Shape; 4] = [Shape::Genesis, Shape::FastForward, Shape::NonFf, Shape::SecondRoot];

/// Who signs every commit the transaction introduces — one signer
/// applies uniformly to keep the state small; per-commit signer
/// variation is Phase 3's concern, not this search's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Signer {
    /// Every introduced commit signed by [`ADMIN_KEY`].
    Admin,
    /// Every introduced commit signed by [`MEMBER_KEY_1`].
    Member,
    /// No introduced commit carries a signature.
    Unsigned,
}

/// All three signer choices, in a fixed enumeration order.
pub const SIGNERS: [Signer; 3] = [Signer::Admin, Signer::Member, Signer::Unsigned];

/// Whether the new tip carries anchor/context retention, and whether it
/// resolves — only meaningful for [`Shape::Genesis`] (a comment-shaped
/// creation); every other shape treats this as absent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Retention {
    /// No anchor or context blob.
    Absent,
    /// Anchor and context present, both resolving.
    Resolving,
    /// Anchor and context present; the anchored blob does not resolve.
    DanglingAnchor,
    /// Anchor and context present; the context blob does not resolve.
    DanglingContext,
}

/// All four retention choices, in a fixed enumeration order.
pub const RETENTIONS: [Retention; 4] =
    [Retention::Absent, Retention::Resolving, Retention::DanglingAnchor, Retention::DanglingContext];

/// All three [`Kind`] choices, in a fixed enumeration order.
pub const KINDS: [Kind; 3] = [Kind::Comment, Kind::Issue, Kind::Effect];

/// A transaction under construction: five independent choices, each
/// `None` until [`SearchModel::actions`] offers it. A state with every
/// field `Some` is complete; [`SearchModel::actions`] then offers
/// nothing further, so the search terminates at exactly 576 leaves.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct State {
    ref_name: Option<&'static str>,
    shape: Option<Shape>,
    signer: Option<Signer>,
    retention: Option<Retention>,
    kind: Option<Kind>,
}

/// One choice, made against exactly one of [`State`]'s five `None`
/// fields, in the fixed order the field declarations above list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    /// Choose the transaction's refname.
    ChooseRef(&'static str),
    /// Choose the transaction's [`Shape`].
    ChooseShape(Shape),
    /// Choose the transaction's [`Signer`].
    ChooseSigner(Signer),
    /// Choose the new tip's [`Retention`].
    ChooseRetention(Retention),
    /// Choose the new tip's signed-content [`Kind`].
    ChooseKind(Kind),
}

/// A fully-chosen transaction, built once all five [`State`] fields are
/// `Some` — the point at which the properties below have anything to
/// check.
pub struct Complete {
    ref_name: &'static str,
    shape: Shape,
    signer: Signer,
    retention: Retention,
    kind: Kind,
}

impl State {
    /// `Some` once every field is chosen, `None` while the transaction
    /// is still under construction — the properties below treat `None`
    /// as vacuously satisfying every obligation, so no discovery is
    /// reported before there is a whole transaction to judge.
    fn complete(&self) -> Option<Complete> {
        Some(Complete {
            ref_name: self.ref_name?,
            shape: self.shape?,
            signer: self.signer?,
            retention: self.retention?,
            kind: self.kind?,
        })
    }
}

impl Complete {
    /// Translate this transaction into `ents_gate_rules::Facts`, exactly
    /// as a real extractor would for each shape — same commit oids and
    /// structure the crate's own unit tests build by hand.
    fn to_facts(&self) -> Facts {
        let mut f = Facts::default();
        enroll_all(&mut f);

        let signed_key = match self.signer {
            Signer::Admin => Some(ADMIN_KEY),
            Signer::Member => Some(MEMBER_KEY_1),
            Signer::Unsigned => None,
        };
        let sign = |f: &mut Facts, oid: &str| {
            if let Some(k) = signed_key {
                f.signed_by.push((oid.to_string(), k.to_string()));
            }
        };

        match self.shape {
            Shape::Genesis => {
                f.ref_update = vec![(self.ref_name.to_string(), None, "g2".to_string())];
                sign(&mut f, "g2");
                match self.retention {
                    Retention::Absent => {}
                    Retention::Resolving => {
                        f.anchor = vec![("g2".to_string(), "blob-a".to_string())];
                        f.context = vec![("g2".to_string(), "blob-ctx".to_string())];
                        f.object_exists = vec![("blob-a".to_string(),), ("blob-ctx".to_string(),)];
                    }
                    Retention::DanglingAnchor => {
                        f.anchor = vec![("g2".to_string(), "blob-a".to_string())];
                        f.context = vec![("g2".to_string(), "blob-ctx".to_string())];
                        f.object_exists = vec![("blob-ctx".to_string(),)];
                    }
                    Retention::DanglingContext => {
                        f.anchor = vec![("g2".to_string(), "blob-a".to_string())];
                        f.context = vec![("g2".to_string(), "blob-ctx".to_string())];
                        f.object_exists = vec![("blob-a".to_string(),)];
                    }
                }
            }
            Shape::FastForward => {
                f.ref_update = vec![(self.ref_name.to_string(), Some("g".to_string()), "c1".to_string())];
                f.parent = vec![("c1".to_string(), "g".to_string())];
                sign(&mut f, "g");
                sign(&mut f, "c1");
            }
            Shape::NonFf => {
                f.ref_update = vec![(self.ref_name.to_string(), Some("g".to_string()), "x".to_string())];
                sign(&mut f, "x");
            }
            Shape::SecondRoot => {
                f.ref_update = vec![(self.ref_name.to_string(), Some("g".to_string()), "m".to_string())];
                f.parent = vec![
                    ("c1".to_string(), "g".to_string()),
                    ("m".to_string(), "c1".to_string()),
                    ("m".to_string(), "z".to_string()),
                ];
                for oid in ["g", "c1", "m", "z"] {
                    sign(&mut f, oid);
                }
            }
        }
        f
    }

    /// `gate(facts).is_empty()` for this transaction — the enabling
    /// condition every property below is stated as a consequent of.
    fn admitted(&self) -> bool {
        gate(self.to_facts()).is_empty()
    }

    /// abstractions.adoc §4 / gate.adoc: an admitted advance descends
    /// from its old tip. [`Shape::NonFf`] is the sole shape that
    /// violates this; every other shape satisfies it by construction.
    fn ff_holds(&self) -> bool {
        !matches!(self.shape, Shape::NonFf)
    }

    /// abstractions.adoc §2 / meta-ref.identity-binding's all-roots
    /// walk: an admitted advance introduces no second parentless commit.
    /// [`Shape::SecondRoot`] is the sole shape that violates this.
    fn single_root_holds(&self) -> bool {
        !matches!(self.shape, Shape::SecondRoot)
    }

    /// abstractions.adoc §5 tip invariant, admission half: every commit
    /// an admitted transaction introduces is signed by an enrolled
    /// member.
    fn tip_signed_holds(&self) -> bool {
        !matches!(self.signer, Signer::Unsigned)
    }

    /// abstractions.adoc §3 / anchor.retention: an admitted genesis's
    /// anchor and context both resolve. Only [`Shape::Genesis`] carries
    /// retention in this model; every other shape is vacuously fine.
    fn retention_holds(&self) -> bool {
        if !matches!(self.shape, Shape::Genesis) {
            return true;
        }
        !matches!(self.retention, Retention::DanglingAnchor | Retention::DanglingContext)
    }

    /// abstractions.adoc §6 / effect.admin-only: an admitted write to
    /// the effects namespace is admin-signed.
    fn effect_admin_holds(&self) -> bool {
        if !self.ref_name.starts_with("refs/meta/effects/") {
            return true;
        }
        matches!(self.signer, Signer::Admin)
    }

    /// abstractions.adoc §2 / meta-ref.identity-binding: the refname's
    /// namespace matches the signed content's own declared
    /// [`Kind`](crate::Kind) — the claim ledger row DIVERGED covers. Only
    /// engages at [`Shape::Genesis`]: binding is a claim about a fresh
    /// entity's placement, not about advancing one that already exists.
    /// [`crate::INBOX_REF`] is deliberately outside this model's binding
    /// scope (Phase 2 obligation 2's allowed second image, not a fresh
    /// binding decision), so it always holds trivially here.
    fn binding_holds(&self) -> bool {
        if !matches!(self.shape, Shape::Genesis) {
            return true;
        }
        let expected = if self.ref_name.starts_with("refs/meta/effects/") {
            Kind::Effect
        } else if self.ref_name.starts_with("refs/meta/comments/") {
            Kind::Comment
        } else if self.ref_name.starts_with("refs/meta/issues/") {
            Kind::Issue
        } else {
            return true;
        };
        self.kind == expected
    }
}

/// The Phase 0.5 search model. Stateless: every choice comes from the
/// bounded universe in `crate`, not from any field here.
pub struct SearchModel;

impl Model for SearchModel {
    type State = State;
    type Action = Action;

    fn init_states(&self) -> Vec<Self::State> {
        vec![State::default()]
    }

    fn actions(&self, state: &Self::State, actions: &mut Vec<Self::Action>) {
        if state.ref_name.is_none() {
            actions.extend(REFS.iter().map(|r| Action::ChooseRef(r)));
        } else if state.shape.is_none() {
            actions.extend(SHAPES.iter().map(|s| Action::ChooseShape(*s)));
        } else if state.signer.is_none() {
            actions.extend(SIGNERS.iter().map(|s| Action::ChooseSigner(*s)));
        } else if state.retention.is_none() {
            actions.extend(RETENTIONS.iter().map(|r| Action::ChooseRetention(*r)));
        } else if state.kind.is_none() {
            actions.extend(KINDS.iter().map(|k| Action::ChooseKind(*k)));
        }
        // A complete state (every field `Some`) offers nothing further:
        // this is a leaf, and the search terminates there.
    }

    fn next_state(&self, last_state: &Self::State, action: Self::Action) -> Option<Self::State> {
        let mut state = last_state.clone();
        match action {
            Action::ChooseRef(r) if state.ref_name.is_none() => state.ref_name = Some(r),
            Action::ChooseShape(s) if state.ref_name.is_some() && state.shape.is_none() => state.shape = Some(s),
            Action::ChooseSigner(s) if state.shape.is_some() && state.signer.is_none() => state.signer = Some(s),
            Action::ChooseRetention(r) if state.signer.is_some() && state.retention.is_none() => {
                state.retention = Some(r);
            }
            Action::ChooseKind(k) if state.retention.is_some() && state.kind.is_none() => state.kind = Some(k),
            _ => return None,
        }
        Some(state)
    }

    fn properties(&self) -> Vec<Property<Self>> {
        /// `gate(facts).is_empty()` implies `check(complete)`, vacuously
        /// true on an incomplete state — one property per doc invariant
        /// the seven denial rules claim to cover, named to match
        /// `verify/alloy/gate_rules.als`'s `check` commands one-to-one.
        fn implication(state: &State, check: fn(&Complete) -> bool) -> bool {
            state.complete().is_none_or(|c| !c.admitted() || check(&c))
        }

        vec![
            Property::always("ff_only_advance", |_, s| implication(s, Complete::ff_holds)),
            Property::always("single_root_identity", |_, s| implication(s, Complete::single_root_holds)),
            Property::always("introduced_commits_member_signed", |_, s| {
                implication(s, Complete::tip_signed_holds)
            }),
            Property::always("anchor_retention_resolves", |_, s| implication(s, Complete::retention_holds)),
            Property::always("effects_writes_admin_signed", |_, s| {
                implication(s, Complete::effect_admin_holds)
            }),
            // The one property expected to have a discovery: the known,
            // ledger-recorded DIVERGED gap. See tests::rediscovers_cross_ref_replay_by_search.
            Property::always("binding_refname_recomputed", |_, s| implication(s, Complete::binding_holds)),
        ]
    }
}

#[cfg(test)]
mod tests {
    use stateright::Checker;

    use super::*;

    /// The migration's acceptance test: search, not hand-construction,
    /// rediscovers the cross-ref replay (ledger row DIVERGED,
    /// `docs/abstractions.adoc` §2). Every other property must have no
    /// discovery — the seven rules are faithful to the other five doc
    /// invariants within this bounded universe.
    #[test]
    fn rediscovers_cross_ref_replay_by_search() {
        let checker = SearchModel.checker().spawn_bfs().join();

        let path = checker.assert_any_discovery("binding_refname_recomputed");
        // Printed so a run's witness can be copied into the PR
        // description as confirmation the harness has teeth.
        println!("binding_refname_recomputed witness: {:?}", path.into_actions());

        checker.assert_no_discovery("ff_only_advance");
        checker.assert_no_discovery("single_root_identity");
        checker.assert_no_discovery("introduced_commits_member_signed");
        checker.assert_no_discovery("anchor_retention_resolves");
        checker.assert_no_discovery("effects_writes_admin_signed");
    }
}
