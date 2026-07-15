//! Rust-native model checking for the formal stocktake (`verify/`,
//! `verify/exercise.md`): [`stateright`] models that call
//! [`ents_gate_rules::gate`] directly, so the refinement mapping between
//! model and code is the function call, not a hand-maintained
//! translation.
//!
//! This crate is a sink in the workspace's layering (`docs/abstractions.
//! adoc`'s "Layering" section, extended by `crates/cli/git-ents/tests/
//! layering.rs`): it depends on [`ents_gate_rules`] and nothing else in
//! the workspace, and nothing in the workspace may depend on it. It
//! exists to be run by `cargo test -p ents-verify`, never linked into a
//! shipped binary.
//!
//! # Modules
//!
//! - [`search`] — the Phase 0.5 replacement: an exhaustive search over a
//!   small bounded universe of transactions, checking that every
//!   admitted transaction (`gate(facts).is_empty()`) also satisfies the
//!   doc invariants the rules are supposed to encode. This is where the
//!   cross-ref replay counterexample (ledger row: DIVERGED,
//!   `docs/abstractions.adoc` §2) is rediscovered by search rather than
//!   asserted by hand.
//! - [`receive`] — Phase 3 skeleton: state/action signatures for the
//!   gate-and-receive protocol, with the gate-check action's enabling
//!   condition wired to the real `gate()` call (the one deliberate
//!   exception to "skeleton, not solution").
//! - [`effects`] — Phase 4 skeleton: trigger/dedup/results state shape.
//! - [`durability`] — Phase 5 skeleton: crash/durability ordering.
//!
//! # The bounded universe
//!
//! Every model in this crate builds transactions from the same tiny,
//! fixed set of atoms — the Rust-native analogue of Alloy's scope
//! discipline (`verify/exercise.md`: "Alloy scopes of 4-6 atoms per
//! signature. Almost every bug in a system like this appears with two
//! members, two refs, three commits."). Keeping every model's universe
//! this small is what makes exhaustive search (Phase 0.5) and bounded
//! model checking (Phases 3-5) tractable at all.

pub mod durability;
pub mod effects;
pub mod receive;
pub mod search;

use ents_gate_rules::{Facts, Role};

/// The one admin-registered key in the bounded universe — the only
/// signer [`effect_admin_violation`](ents_gate_rules) and the
/// redaction-admin-only rule (`docs/spec/receive.adoc`
/// `receive.redaction-admin-only`) accept for their namespaces.
pub const ADMIN_KEY: &str = "key:admin";
/// An ordinary enrolled, non-admin member key.
pub const MEMBER_KEY_1: &str = "key:m1";
/// A second ordinary member key — the bounded universe's "two members"
/// atom, needed for adoption/divergence scenarios where a single key
/// isn't enough to tell two actors apart.
pub const MEMBER_KEY_2: &str = "key:m2";

/// Every key in the bounded universe, admin first.
pub const KEYS: [&str; 3] = [ADMIN_KEY, MEMBER_KEY_1, MEMBER_KEY_2];

/// The fixed role a key in the bounded universe carries. Roles are not
/// an independent search dimension here: enrolling a key is a single
/// fact (present or absent), never a choice of role, exactly so search
/// states can dedupe on plain sets instead of tracking role assignment
/// as extra state — [`ents_gate_rules::Role`] itself has no `Ord`, which
/// would otherwise complicate that dedup.
#[must_use]
pub fn role_of(key: &str) -> Role {
    if key == ADMIN_KEY {
        Role::Admin
    } else {
        Role::Member
    }
}

/// A hash-identified namespace (`docs/spec/meta-ref.adoc`
/// `meta-ref.identity-binding`) — genesis-oid binding, the shape
/// `issues/*` and `comments/*` share.
pub const ISSUE_REF: &str = "refs/meta/issues/g";
/// The other hash-identified namespace in the bounded universe, kept
/// distinct from [`ISSUE_REF`] so a model can distinguish "which
/// hash-identified namespace" without adding a third dimension.
pub const COMMENT_REF: &str = "refs/meta/comments/g2";
/// The admin-only namespace (`effect.admin-only`) — the one the crate's
/// `effect_admin_violation` rule protects, and the namespace the known
/// cross-ref replay counterexample targets.
pub const EFFECT_REF: &str = "refs/meta/effects/x";
/// The one namespace `meta-ref.inbox` declares as an allowed *second*
/// image of an already-bound signed commit — Phase 2 obligation 2.
pub const INBOX_REF: &str = "refs/meta/inbox/m1/comments/g2";

/// Every refname in the bounded universe.
pub const REFS: [&str; 4] = [ISSUE_REF, COMMENT_REF, EFFECT_REF, INBOX_REF];

/// Object ids in the bounded universe: enough to build a genesis
/// (`g2`), a fast-forward advance of an existing tip (`g` -> `c1`), and
/// a two-parent merge that smuggles in an unrelated root (`z`) — the
/// three transaction shapes `ents_gate_rules`' own unit tests already
/// exercise by hand, plus two blobs for anchor/context retention.
pub const OIDS: [&str; 6] = ["g", "c1", "m", "z", "blob-a", "blob-ctx"];

/// The signed content's own kind, as the *author's* signed content
/// declares it — the derivation input `docs/abstractions.adoc` §2 says
/// the refname recomputes from. Deliberately absent from
/// [`ents_gate_rules::Facts`] itself: that absence is the gap under
/// test. Every model that checks the binding invariant carries this as a
/// shadow annotation alongside a built [`Facts`] value, never inside it,
/// exactly mirroring `verify/alloy/gate_rules.als`'s `kind` field.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    /// The signed content is a comment (`docs/spec/model.adoc`
    /// `model.comment`).
    Comment,
    /// The signed content is an issue (`model.issue`).
    Issue,
    /// The signed content is an effect definition (`model.effect-
    /// definition`, `effect.admin-only`).
    Effect,
}

/// Enroll every key in [`KEYS`] into `facts.member`, at its fixed
/// [`role_of`]. Every model in this crate treats membership as
/// background, not a search dimension — Phase 3's `receive` skeleton is
/// where membership *lifecycle* (enrollment, revocation) belongs.
pub fn enroll_all(facts: &mut Facts) {
    facts.member = KEYS.iter().map(|k| ((*k).to_string(), role_of(k))).collect();
}
