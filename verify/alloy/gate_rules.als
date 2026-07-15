// Phase 0.5 — verify the verifier (verify/exercise.md, "Phase 0.5").
//
// This file is NOT a stub. It translates the seven denial rules of
// crates/kernel/ents-gate-rules/src/lib.rs into Alloy, one predicate per
// rule, same names, and then runs the check the crate cannot run on
// itself: search for transactions with zero violations that break a doc
// invariant (docs/abstractions.adoc; docs/spec/meta-ref.adoc,
// meta-ref.identity-binding; docs/spec/gate.adoc).
//
// The crate is ground truth for the translation: every signature below
// mirrors one EDB relation of ents-gate-rules one-to-one, and every
// denial predicate is a mechanical transcription of the corresponding
// ascent rule. The one extra field, `kind`, models what the *signed
// content* of a commit says it is — the derivation input for refname
// recomputation. It is deliberately absent from the rule vocabulary,
// because that absence is the gap under test.
//
// Expected outcome: every check passes EXCEPT binding_refname_recomputed,
// which must produce the known cross-ref replay counterexample (an
// admin-signed parentless comment commit created at refs/meta/effects/x).
// That failure is the harness working, not the harness broken. It is
// pinned on the code side by crates/kernel/ents-gate-rules/tests/ledger.rs
// and recorded in verify/ledger.adoc (verdict DIVERGED).

module gate_rules

// ---- EDB vocabulary, one signature/field per ents-gate-rules relation ----

// Oid: an object id. `parent`, `signed_by`, `anchor`, `context` mirror the
// crate's relations parent(Oid, Oid), signed_by(Oid, Key), anchor(Oid, Oid),
// context(Oid, Oid).
sig Oid {
  parent: set Oid,
  signed_by: set Key,
  anchor: set Oid,
  context: set Oid,
  // NOT part of the crate's vocabulary: the entity kind the signed
  // content carries, from which meta-ref.identity-binding says the
  // refname recomputes. Modeling it here is what lets Alloy state the
  // doc invariant the rules do not check.
  kind: lone Kind
}

// member(Key, Role): a key is enrolled iff `role` is nonempty.
sig Key { role: lone Role }
abstract sig Role {}
one sig Admin, Member extends Role {}

abstract sig Kind {}
one sig CommentKind, IssueKind, EffectKind extends Kind {}

// Refnames, abstracted to the one distinction the rules make:
// `r.starts_with("refs/meta/effects/")`.
abstract sig RefName {}
sig EffectsRef, OtherRef extends RefName {}

// object_exists(Oid): objects the repository already has, or that arrive
// in this pack.
one sig Store { object_exists: set Oid }

// ref_update(Ref, Option<Oid>, Oid): `no old` is entity creation.
sig RefUpdate {
  ref: one RefName,
  old: lone Oid,
  new: one Oid
}

// ---- IDB: derived relations, transcribed ----

// ancestor: transitive ancestry.
fun ancestors[c: Oid]: set Oid { c.^parent }

// has_parent(Oid)
pred has_parent[c: Oid] { some c.parent }

// covered(Ref, Oid): commits already covered by a ref's old tip.
fun covered[u: RefUpdate]: set Oid { u.old + ancestors[u.old] }

// introduced(Ref, Oid): the new tip and its ancestors, minus everything
// the old tip already reached.
fun introduced[u: RefUpdate]: set Oid { (u.new + ancestors[u.new]) - covered[u] }

// member_signed(Oid)
pred member_signed[c: Oid] { some k: c.signed_by | some k.role }

// admin_signed(Oid)
pred admin_signed[c: Oid] { some k: c.signed_by | k.role = Admin }

// ---- The seven denial rules, same names as the crate ----

// Fast-forward-only: the new tip must descend from the old tip.
pred ff_violation[u: RefUpdate] {
  some u.old and u.old != u.new and u.old not in ancestors[u.new]
}

// Creation must point at a parentless genesis commit.
pred genesis_violation[u: RefUpdate] {
  no u.old and has_parent[u.new]
}

// One entity, one root: past genesis, an update may not introduce a
// second parentless commit.
pred second_root_violation[u: RefUpdate] {
  some u.old and some c: introduced[u] | not has_parent[c]
}

// Every introduced commit must carry a signature from a currently
// enrolled member.
pred unsigned_violation[u: RefUpdate] {
  some c: introduced[u] | not member_signed[c]
}

// An anchored blob must resolve to an object the repository will contain.
pred dangling_anchor_violation[u: RefUpdate] {
  some c: introduced[u] | some (c.anchor - Store.object_exists)
}

// The paired context blob must resolve too.
pred dangling_context_violation[u: RefUpdate] {
  some c: introduced[u] | some (c.context - Store.object_exists)
}

// A write to refs/meta/effects/* must be signed by an admin-registered
// member.
pred effect_admin_violation[u: RefUpdate] {
  u.ref in EffectsRef and some c: introduced[u] | not admin_signed[c]
}

// admitted: the crate's `gate(facts).is_empty()` — no denial relation
// holds any row for this update.
pred admitted[u: RefUpdate] {
  not ff_violation[u]
  not genesis_violation[u]
  not second_root_violation[u]
  not unsigned_violation[u]
  not dangling_anchor_violation[u]
  not dangling_context_violation[u]
  not effect_admin_violation[u]
}

// ---- Checks: one per doc invariant the rules claim to cover ----

// abstractions §4 / gate: fast-forward-only advance (ff_violation).
assert ff_only_advance {
  all u: RefUpdate | (admitted[u] and some u.old and u.old != u.new)
    implies u.old in ancestors[u.new]
}
check ff_only_advance for 6

// abstractions §2 / meta-ref.identity-binding all-roots walk: an admitted
// update never introduces a second parentless commit
// (genesis_violation + second_root_violation).
assert single_root_identity {
  all u: RefUpdate | (admitted[u] and some u.old)
    implies (no c: introduced[u] | not has_parent[c])
}
check single_root_identity for 6

// abstractions §5 tip invariant, admission half: every commit an admitted
// transaction introduces is member-signed (unsigned_violation).
assert introduced_commits_member_signed {
  all u: RefUpdate | admitted[u]
    implies (all c: introduced[u] | member_signed[c])
}
check introduced_commits_member_signed for 6

// abstractions §3 / anchor.retention: both embedded objects of every
// introduced anchor resolve (dangling_anchor_violation +
// dangling_context_violation).
assert anchor_retention_resolves {
  all u: RefUpdate | admitted[u]
    implies (all c: introduced[u] | (c.anchor + c.context) in Store.object_exists)
}
check anchor_retention_resolves for 6

// abstractions §6 / effect.admin-only: an admitted write to
// refs/meta/effects/* is admin-signed (effect_admin_violation).
assert effects_writes_admin_signed {
  all u: RefUpdate | (admitted[u] and u.ref in EffectsRef)
    implies (all c: introduced[u] | admin_signed[c])
}
check effects_writes_admin_signed for 6

// abstractions §4 / meta-ref.identity-binding: "the refname is a total
// function of signed content, recomputed at verification." No rule in
// ents-gate-rules covers this, and no gap marker declares the omission.
// EXPECTED TO FAIL with the cross-ref replay counterexample: an
// admin-signed parentless commit whose signed content is a comment
// (kind = CommentKind, anchor + context present and resolving), replayed
// as the creation of an effects ref — genesis, unsigned, and effect_admin
// are all satisfied, ff vacuously. Ledger row: DIVERGED.
assert binding_refname_recomputed {
  all u: RefUpdate | (admitted[u] and no u.old and u.ref in EffectsRef)
    implies u.new.kind = EffectKind
}
check binding_refname_recomputed for 6
