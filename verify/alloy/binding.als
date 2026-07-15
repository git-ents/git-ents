// Phase 2 — refname binding as a total function (verify/exercise.md,
// "Phase 2").
//
// STUB. This file parses, declares the vocabulary, and names one
// derivation predicate per meta-ref namespace — it checks NOTHING. Every
// predicate body is deliberately empty (trivially true) until the human
// exercise writes each derivation from the code, not from memory.
//
// Discharges, once filled in: docs/abstractions.adoc §4 ("the refname is
// a total function of signed content, recomputed at verification");
// docs/spec/meta-ref.adoc meta-ref.identity-binding and meta-ref.inbox.
// The namespace list below is enumerated from meta-ref.adoc's own
// binding taxonomy: fixed-name singletons, natural-key, hash-identified,
// composite-keyed, inbox/self signer-bound, and pins.
//
// Vocabulary: signatures mirror the EDB relations of
// crates/kernel/ents-gate-rules/src/lib.rs one-to-one, so the binding
// model composes with gate_rules.als — the composition is exactly the
// cross-ref replay check (Phase 4, obligation 3).

module binding

// ---- EDB vocabulary, one signature/field per ents-gate-rules relation ----

sig Oid {
  parent: set Oid,      // parent(child, parent)
  signed_by: set Key,   // signed_by(commit, key)
  anchor: set Oid,      // anchor(entity commit, anchored blob)
  context: set Oid      // context(entity commit, context blob)
}

sig Key { role: lone Role }   // member(Key, Role)
abstract sig Role {}
one sig Admin, Member extends Role {}

abstract sig RefName {}
sig EffectsRef, OtherRef extends RefName {}

one sig Store { object_exists: set Oid }   // object_exists(Oid)

sig RefUpdate {                // ref_update(Ref, Option<Oid>, Oid)
  ref: one RefName,
  old: lone Oid,
  new: one Oid
}

// The binding function under study: refname derived from signed content.
// The exercise fills in its definition per namespace; here it is a free
// relation so the file parses.
sig Binding { binds: Oid -> lone RefName }

// ---- Per-namespace derivation stubs (meta-ref.identity-binding) ----
// Each states, once written, how that namespace's refname derives from
// signed content. All STUBS — they check nothing.

// refs/meta/account — fixed name (singleton state).
pred binding_account {
  // TODO(exercise)
}

// refs/meta/config — fixed name (singleton state).
pred binding_config {
  // TODO(exercise)
}

// refs/meta/member/* — natural key: designated tree field equals the
// refname's final segment.
pred binding_member {
  // TODO(exercise)
}

// refs/meta/effects/* — natural key: the effect's name.
pred binding_effects {
  // TODO(exercise)
}

// refs/meta/issues/* — hash-identified: final segment equals the genesis
// commit oid; all parentless commits reachable from the tip are that
// genesis.
pred binding_issues {
  // TODO(exercise)
}

// refs/meta/comments/* — hash-identified, same rule as issues.
pred binding_comments {
  // TODO(exercise)
}

// refs/meta/reviews/<target>/<member> — composite-keyed: genesis tree's
// target field + genesis signer's member id.
pred binding_reviews {
  // TODO(exercise)
}

// refs/meta/results/<effect>/<short-oid> — composite-keyed: derived from
// the result's own tree fields.
pred binding_results {
  // TODO(exercise)
}

// refs/meta/inbox/<member>/<canonical-suffix> — owner segment equals the
// signer; suffix bound as its canonical namespace binds.
pred binding_inbox {
  // TODO(exercise)
}

// refs/meta/self/<member>/<effect>/<short-oid> — member segment equals
// the signer, mirroring the canonical results pattern.
pred binding_self {
  // TODO(exercise)
}

// refs/meta/pins/* — mirrors its entity's segments; parentless-roots walk
// deliberately not applied.
pred binding_pins {
  // TODO(exercise)
}

// ---- Phase 2 obligation stubs ----

// Obligation 1 aggregate: the binding is a TOTAL function over every
// namespace above. STUB — checks nothing.
pred binding_total_function {
  // TODO(exercise)
}

// Obligation 2: inbox is the one allowed second image of the same signed
// commit; nothing else is. STUB — checks nothing.
pred inbox_allowed_second_image {
  // TODO(exercise)
}

// Obligation 3: self/<member> derives from the SIGNATURE, not a tree
// field an author could forge. STUB — checks nothing.
pred self_member_from_signature {
  // TODO(exercise)
}

// Obligation 4: is repo identity anywhere in signed content? Cross-repo
// replay. CONDITIONAL either way — write the condition. STUB — checks
// nothing.
pred cross_repo_replay {
  // TODO(exercise)
}

// Parse-only smoke command so `check-alloy` has something to execute;
// it asserts nothing about the system.
run { some Binding } for 3
