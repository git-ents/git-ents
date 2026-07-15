// Phase 1 — the object-graph substrate (verify/exercise.md, "Phase 1").
//
// STUB. This file is scaffolding for a paper-first exercise: it parses,
// it declares the vocabulary, and it names the obligations — it checks
// NOTHING. Every predicate below is deliberately empty (trivially true)
// until a human writes the model longhand and transcribes it here.
//
// Discharges, once filled in: docs/abstractions.adoc §2 (typed tree,
// schema pinning), §3 (anchor retention, redaction); docs/spec/anchor.adoc
// anchor.retention; the schema-pinning string-OID/gitlink negatives.
//
// Vocabulary: the signatures mirror the EDB relations of
// crates/kernel/ents-gate-rules/src/lib.rs one-to-one (ref_update, parent,
// signed_by, member, anchor, context, object_exists), so a claim proved
// here composes with gate_rules.als without renaming. Phase 1 will refine
// Oid into Blob + Tree + Commit with tree entries; that refinement is the
// human's first move, not the harness's.

module objects

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

// ---- Obligation stubs: named, empty, trivially true. Not checked. ----

// Obligation 1: entity tree embeds schema as a real entry => schema in
// reach[entityTip]. STUB — checks nothing.
pred schema_pinning {
  // TODO(exercise)
}

// Obligation 1, negative: the string-OID variant does NOT retain the
// schema (reachability fails). STUB — checks nothing.
pred schema_pinning_string_oid_fails {
  // TODO(exercise)
}

// Obligation 1, negative: the gitlink variant does NOT retain the schema.
// STUB — checks nothing.
pred schema_pinning_gitlink_fails {
  // TODO(exercise)
}

// Obligation 2: embedded anchored blob + context blob reachable from the
// meta-ref (anchor.retention). STUB — checks nothing.
pred anchor_retention {
  // TODO(exercise)
}

// Obligation 2, degraded trace: anchored commit gc'd, anchor still
// projects from its embedded objects. STUB — checks nothing.
pred anchor_degraded_projection {
  // TODO(exercise)
}

// Obligation 3: redaction as object withholding — can withholding one
// entity's blob break another entity's closure via dedup sharing?
// CONDITIONAL candidate. STUB — checks nothing.
pred redaction_vs_retention {
  // TODO(exercise)
}

// Parse-only smoke command so `check-alloy` has something to execute;
// it asserts nothing about the system.
run { some Oid } for 3
