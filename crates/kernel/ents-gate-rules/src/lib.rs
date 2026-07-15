//! The six abstractions (`docs/abstractions.adoc`), the ones expressible as
//! facts about a proposed ref transaction, restated as compiled Datalog.
//!
//! Every load-bearing rule here is a statement over facts the real
//! extractor would pull from a pack plus current repository state: which
//! refs move from what to what, which commits exist with which parents,
//! who signed what, which keys are enrolled members. [`ascent`] embeds
//! Datalog in Rust via a proc macro, so rustc checks the rules' types,
//! arities, variable bindings, and stratification; a violation is simply a
//! non-empty relation, and [`gate`] collects them.
//!
//! # Why this is a separate crate from `ents-gate`
//!
//! `ents-gate` is the one pure admission judgment actually wired into the
//! three real call sites (hosted CAS, local UI verdict, push pre-flight;
//! its own module docs cite `gate.call-sites`) — it reads a live
//! `RefStoreRead`, decodes typed trees, and renders actionable refusals.
//! This crate consumes none of that; it takes plain facts and is meant to
//! be cheap to grow one denial rule at a time while an invariant is still
//! being worked out, exactly as the source technique note that motivated
//! this crate puts it: rules here are fixed at compile time, which is a
//! feature for load-bearing, human-authored invariants, not a runtime
//! query surface over live entity data.
//!
//! It is not itself one of the three enforcement points today. Carrying a
//! rule proven out here into `ents-gate`'s actual fact extraction (or
//! `ents-effect`'s trigger/dedup bookkeeping) is future work, one rule at
//! a time, the same way the technique note describes: "a rule without a
//! red test is a rule you don't know fires."
//!
//! # Coverage and gaps
//!
//! Five rules restate abstractions 2, 3, 4, and 5 directly:
//!
//! - [`ff_violation`](GateRules::ff_violation) — fast-forward-only advance
//!   is the anti-replay binding a signed commit relies on (abstraction 4).
//! - [`genesis_violation`](GateRules::genesis_violation) and
//!   [`second_root_violation`](GateRules::second_root_violation) — a
//!   hash-identified entity's ref has exactly one parentless commit
//!   reachable from its tip (abstraction 2's typed tree, `meta-ref.
//!   identity-binding`'s all-roots walk).
//! - [`unsigned_violation`](GateRules::unsigned_violation) — every commit
//!   a transaction introduces must carry a member signature (abstraction
//!   5's tip invariant).
//! - [`dangling_anchor_violation`](GateRules::dangling_anchor_violation)
//!   and [`dangling_context_violation`](GateRules::dangling_context_violation)
//!   — an anchor's embedded retention is two objects, not one: the
//!   anchored blob and a context blob of the surrounding lines
//!   (abstraction 3, `anchor.retention`); both must resolve.
//!
//! One rule grows the set past the original five, following the technique
//! note's own suggested next step ("role-scoped authorization,
//! `member(Key, Role)` plus per-namespace requirements"):
//!
//! - [`effect_admin_violation`](GateRules::effect_admin_violation) — a
//!   commit introduced onto `refs/meta/effects/*` must be signed by an
//!   admin-registered member, never merely any member (abstraction 6,
//!   `effect.admin-only`: authoring an effect schedules code execution on
//!   canonical infrastructure, which needs more trust than an ordinary
//!   append).
//!
//! Two invariants are deliberately *not* encoded here, the gap marked
//! rather than papered over:
//!
//! - Abstraction 1's granularity rule ("one ref per independently-authored
//!   entity") is a ref-layout convention checked by which refname a write
//!   targets, not a property of the commits within one transaction's
//!   facts — it has no shape as a per-transaction Datalog fact here.
//! - Abstraction 6's monotone, exactly-once effect semantics needs the
//!   dedup key `(effect, oid)` checked against the results namespace
//!   *across* transactions and time, which is queue/materialization state
//!   this crate's fact set does not carry.
//!
//! # Examples
//!
//! ```
//! use ents_gate_rules::{Facts, Role, gate};
//!
//! let mut facts = Facts {
//!     member: vec![("key:joey".into(), Role::Member)],
//!     ..Facts::default()
//! };
//! facts.ref_update = vec![("refs/meta/issues/g".into(), Some("g".into()), "c1".into())];
//! facts.parent = vec![("c1".into(), "g".into())];
//! facts.signed_by = vec![("g".into(), "key:joey".into()), ("c1".into(), "key:joey".into())];
//! assert!(gate(facts).is_empty());
//! ```

use ascent::ascent;

/// An object id, standing in for `gix_hash::ObjectId` — a plain `String`
/// here so a rule's facts stay readable in tests, the same simplification
/// the technique note that motivated this crate makes with `&'static
/// str`; any `Clone + Eq + Hash` type works once this is wired to a real
/// extractor.
pub type Oid = String;
/// A refname, standing in for `gix::refs::FullName`.
pub type Ref = String;
/// A signing key's identity, standing in for a member's enrolled public
/// key material.
pub type Key = String;

/// A member's provenance, exactly the two cases `ents_model::Provenance`
/// carries — kept as a local, minimal fact rather than a dependency on
/// `ents-model` itself, so this crate stays a standalone place to iterate
/// on invariants rather than a second consumer of the kernel's real
/// types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    /// Enrolled by an admin-registered member — the only provenance
    /// `effect.admin-only` accepts for a write to `refs/meta/effects/*`.
    Admin,
    /// Any other enrolled member, admin-registered or self-attested, for
    /// rules that only need "signed by someone currently enrolled."
    Member,
}

ascent! {
    /// The compiled rule set: an `ascent`-generated struct whose fields
    /// are `Vec`-backed relations, populated from [`Facts`] and run to a
    /// fixpoint by [`gate`]. Not part of this crate's public surface —
    /// [`Facts`] and [`gate`] are the two things a caller needs.
    struct GateRules;

    // ---- EDB: facts a real extractor would pull from the pack, the
    // proposed ref transaction, and current repository state ----

    /// Proposed ref transaction: (ref, old tip, new tip). `None` old tip
    /// means entity creation.
    relation ref_update(Ref, Option<Oid>, Oid);
    /// (child, parent) commit edges for the new tips' ancestry, bounded at
    /// the old tips — the frontier the update can reach beyond what the
    /// old tip already covers.
    relation parent(Oid, Oid);
    /// (commit, signing key), emitted only after signature verification
    /// succeeds — the crypto lives in the extractor, never in a rule.
    relation signed_by(Oid, Key);
    /// Keys currently enrolled, and each one's provenance.
    relation member(Key, Role);
    /// (entity commit, anchored blob) — the first of the two objects
    /// `anchor.retention` requires a comment (or any anchor consumer) to
    /// embed.
    relation anchor(Oid, Oid);
    /// (entity commit, context blob) — the second embedded object
    /// `anchor.retention` requires: a context blob of the surrounding
    /// source lines, written fresh alongside the anchored blob.
    relation context(Oid, Oid);
    /// Objects the repository already has, or that arrive in this pack.
    relation object_exists(Oid);

    // ---- IDB: derived relations ----

    /// Transitive ancestry.
    relation ancestor(Oid, Oid);
    ancestor(c.clone(), p.clone()) <-- parent(c, p);
    ancestor(c.clone(), a.clone()) <-- parent(c, p), ancestor(p, a);

    /// Whether a commit has any recorded parent.
    relation has_parent(Oid);
    has_parent(c.clone()) <-- parent(c, _p);

    /// Commits already covered by a ref's old tip.
    relation covered(Ref, Oid);
    covered(r.clone(), o.clone()) <--
        ref_update(r, old, _new), if let Some(o) = old;
    covered(r.clone(), a.clone()) <--
        ref_update(r, old, _new), if let Some(o) = old, ancestor(o, a);

    /// Commits this transaction introduces to a ref: the new tip and its
    /// ancestors, minus everything the old tip already reached.
    relation introduced(Ref, Oid);
    introduced(r.clone(), n.clone()) <--
        ref_update(r, _old, n), !covered(r, n);
    introduced(r.clone(), c.clone()) <--
        ref_update(r, _old, n), ancestor(n, c), !covered(r, c);

    /// A commit signed by any currently enrolled member, of any
    /// provenance.
    relation member_signed(Oid);
    member_signed(c.clone()) <-- signed_by(c, k), member(k, _role);

    /// A commit signed by an admin-registered member specifically.
    relation admin_signed(Oid);
    admin_signed(c.clone()) <-- signed_by(c, k), member(k, Role::Admin);

    // ---- Denial rules: any row here rejects the transaction ----

    /// Fast-forward-only: the new tip must descend from the old tip.
    relation ff_violation(Ref);
    ff_violation(r.clone()) <--
        ref_update(r, old, new), if let Some(o) = old,
        if o != new, !ancestor(new, o);

    /// Creation must point at a parentless genesis commit.
    relation genesis_violation(Ref);
    genesis_violation(r.clone()) <--
        ref_update(r, old, new), if old.is_none(), has_parent(new);

    /// One entity, one root: past genesis, an update may not introduce a
    /// second parentless commit — merging in an unrelated chain would
    /// satisfy fast-forward while smuggling in a doppelgänger identity.
    relation second_root_violation(Ref, Oid);
    second_root_violation(r.clone(), c.clone()) <--
        ref_update(r, old, _new), if old.is_some(),
        introduced(r, c), !has_parent(c);

    /// Every introduced commit must carry a signature from a currently
    /// enrolled member.
    relation unsigned_violation(Ref, Oid);
    unsigned_violation(r.clone(), c.clone()) <--
        introduced(r, c), !member_signed(c);

    /// An anchored blob must resolve to an object the repository will
    /// contain.
    relation dangling_anchor_violation(Ref, Oid);
    dangling_anchor_violation(r.clone(), t.clone()) <--
        introduced(r, c), anchor(c, t), !object_exists(t);

    /// The paired context blob must resolve too — `anchor.retention`
    /// requires both, not only the anchored blob.
    relation dangling_context_violation(Ref, Oid);
    dangling_context_violation(r.clone(), t.clone()) <--
        introduced(r, c), context(c, t), !object_exists(t);

    /// A write to `refs/meta/effects/*` must be signed by an
    /// admin-registered member, regardless of any other role rule
    /// (`effect.admin-only`).
    relation effect_admin_violation(Ref, Oid);
    effect_admin_violation(r.clone(), c.clone()) <--
        introduced(r, c), if r.starts_with("refs/meta/effects/"),
        !admin_signed(c);
}

/// Facts for one proposed transaction. In the real system these would be
/// extracted with gix from the pack and the current ref/member state; here
/// they are supplied directly so a rule's behavior can be pinned by a
/// test.
#[derive(Debug, Clone, Default)]
pub struct Facts {
    /// See [`GateRules::ref_update`].
    pub ref_update: Vec<(Ref, Option<Oid>, Oid)>,
    /// See [`GateRules::parent`].
    pub parent: Vec<(Oid, Oid)>,
    /// See [`GateRules::signed_by`].
    pub signed_by: Vec<(Oid, Key)>,
    /// See [`GateRules::member`].
    pub member: Vec<(Key, Role)>,
    /// See [`GateRules::anchor`].
    pub anchor: Vec<(Oid, Oid)>,
    /// See [`GateRules::context`].
    pub context: Vec<(Oid, Oid)>,
    /// See [`GateRules::object_exists`].
    pub object_exists: Vec<(Oid,)>,
}

/// Run every denial rule to a fixpoint over `facts`. An empty result means
/// the transaction is admitted under every invariant this crate currently
/// states.
#[must_use]
pub fn gate(facts: Facts) -> Vec<String> {
    let mut rules = GateRules {
        ref_update: facts.ref_update,
        parent: facts.parent,
        signed_by: facts.signed_by,
        member: facts.member,
        anchor: facts.anchor,
        context: facts.context,
        object_exists: facts.object_exists,
        ..GateRules::default()
    };
    rules.run();

    let mut out = Vec::new();
    for (r,) in &rules.ff_violation {
        out.push(format!("ff: {r}: new tip does not descend from old tip"));
    }
    for (r,) in &rules.genesis_violation {
        out.push(format!("genesis: {r}: creation tip has parents"));
    }
    for (r, c) in &rules.second_root_violation {
        out.push(format!("root: {r}: introduces second root {c}"));
    }
    for (r, c) in &rules.unsigned_violation {
        out.push(format!(
            "signature: {r}: {c} not signed by an enrolled member"
        ));
    }
    for (r, t) in &rules.dangling_anchor_violation {
        out.push(format!("anchor: {r}: anchored object {t} does not exist"));
    }
    for (r, t) in &rules.dangling_context_violation {
        out.push(format!("context: {r}: context object {t} does not exist"));
    }
    for (r, c) in &rules.effect_admin_violation {
        out.push(format!(
            "effect-admin: {r}: {c} not signed by an admin-registered member"
        ));
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const ISSUE: &str = "refs/meta/issues/g";
    const COMMENT: &str = "refs/meta/comments/g2";
    const EFFECT: &str = "refs/meta/effects/ci";

    fn base() -> Facts {
        Facts {
            member: vec![("key:joey".into(), Role::Member)],
            ..Facts::default()
        }
    }

    #[test]
    fn creation_and_ff_update_pass() {
        // genesis g, then g <- c1 pushed as an update.
        let mut f = base();
        f.ref_update = vec![(ISSUE.into(), Some("g".into()), "c1".into())];
        f.parent = vec![("c1".into(), "g".into())];
        f.signed_by = vec![
            ("g".into(), "key:joey".into()),
            ("c1".into(), "key:joey".into()),
        ];
        assert!(gate(f).is_empty());

        let mut f = base();
        f.ref_update = vec![(COMMENT.into(), None, "g2".into())];
        f.signed_by = vec![("g2".into(), "key:joey".into())];
        f.anchor = vec![("g2".into(), "blob:a".into())];
        f.context = vec![("g2".into(), "blob:ctx".into())];
        f.object_exists = vec![("blob:a".into(),), ("blob:ctx".into(),)];
        assert!(gate(f).is_empty());
    }

    #[test]
    fn non_ff_is_rejected() {
        let mut f = base();
        f.ref_update = vec![(ISSUE.into(), Some("g".into()), "x".into())]; // x unrelated to g
        f.signed_by = vec![("x".into(), "key:joey".into())];
        let v = gate(f);
        assert!(v.iter().any(|m| m.starts_with("ff:")), "{v:?}");
    }

    #[test]
    fn parented_genesis_is_rejected() {
        let mut f = base();
        f.ref_update = vec![(ISSUE.into(), None, "c1".into())];
        f.parent = vec![("c1".into(), "elsewhere".into())];
        f.signed_by = vec![("c1".into(), "key:joey".into())];
        let v = gate(f);
        assert!(v.iter().any(|m| m.starts_with("genesis:")), "{v:?}");
    }

    #[test]
    fn merged_in_second_root_is_rejected() {
        // old tip g; new tip m is a merge of c1 (descends from g) and z
        // (an unrelated parentless chain). FF holds; root rule fires.
        let mut f = base();
        f.ref_update = vec![(ISSUE.into(), Some("g".into()), "m".into())];
        f.parent = vec![
            ("c1".into(), "g".into()),
            ("m".into(), "c1".into()),
            ("m".into(), "z".into()),
        ];
        f.signed_by = vec![
            ("c1".into(), "key:joey".into()),
            ("m".into(), "key:joey".into()),
            ("z".into(), "key:joey".into()),
        ];
        let v = gate(f);
        assert!(
            v.iter().any(|m| m.starts_with("root:") && m.contains('z')),
            "{v:?}"
        );
        assert!(!v.iter().any(|m| m.starts_with("ff:")), "{v:?}");
    }

    #[test]
    fn non_member_signature_is_rejected() {
        let mut f = base();
        f.ref_update = vec![(ISSUE.into(), Some("g".into()), "c1".into())];
        f.parent = vec![("c1".into(), "g".into())];
        f.signed_by = vec![("c1".into(), "key:mallory".into())];
        let v = gate(f);
        assert!(
            v.iter()
                .any(|m| m.starts_with("signature:") && m.contains("c1")),
            "{v:?}"
        );
    }

    #[test]
    fn dangling_anchor_is_rejected() {
        let mut f = base();
        f.ref_update = vec![(COMMENT.into(), None, "g2".into())];
        f.signed_by = vec![("g2".into(), "key:joey".into())];
        f.anchor = vec![("g2".into(), "blob:missing".into())];
        f.context = vec![("g2".into(), "blob:ctx".into())];
        f.object_exists = vec![("blob:ctx".into(),)];
        let v = gate(f);
        assert!(v.iter().any(|m| m.starts_with("anchor:")), "{v:?}");
    }

    #[test]
    fn dangling_context_is_rejected() {
        let mut f = base();
        f.ref_update = vec![(COMMENT.into(), None, "g2".into())];
        f.signed_by = vec![("g2".into(), "key:joey".into())];
        f.anchor = vec![("g2".into(), "blob:a".into())];
        f.context = vec![("g2".into(), "blob:missing-ctx".into())];
        f.object_exists = vec![("blob:a".into(),)];
        let v = gate(f);
        assert!(v.iter().any(|m| m.starts_with("context:")), "{v:?}");
    }

    #[test]
    fn effect_definition_by_admin_passes() {
        let mut f = Facts {
            member: vec![("key:admin".into(), Role::Admin)],
            ..Facts::default()
        };
        f.ref_update = vec![(EFFECT.into(), None, "e1".into())];
        f.signed_by = vec![("e1".into(), "key:admin".into())];
        assert!(gate(f).is_empty());
    }

    #[test]
    fn effect_definition_by_non_admin_is_rejected() {
        // Signed by a currently enrolled member, so `unsigned_violation`
        // does not fire — only the effects-specific admin rule should.
        let mut f = base();
        f.ref_update = vec![(EFFECT.into(), None, "e1".into())];
        f.signed_by = vec![("e1".into(), "key:joey".into())];
        let v = gate(f);
        assert!(v.iter().any(|m| m.starts_with("effect-admin:")), "{v:?}");
        assert!(!v.iter().any(|m| m.starts_with("signature:")), "{v:?}");
    }
}
