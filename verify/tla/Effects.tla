---------------------------- MODULE Effects ----------------------------
(***************************************************************************)
(* Phase 4 — effects (verify/exercise.md, "Phase 4").                      *)
(*                                                                         *)
(* SKELETON. Variables and action signatures only; every body is a        *)
(* TODO(exercise) stub that changes no state. This module checks NOTHING  *)
(* until the human exercise fills it in on top of Phase 3's state.        *)
(*                                                                         *)
(* Discharges, once filled in: docs/abstractions.adoc §6 (monotone,       *)
(* exactly-once effects); docs/spec/effect.adoc (trigger set, dedup key   *)
(* (effect, refname, new_oid), results write-back, admin-only authoring). *)
(* Note ents-gate-rules marks the cross-transaction dedup obligation as   *)
(* a deliberate gap — this model is where that obligation lives.          *)
(*                                                                         *)
(* Vocabulary: the transaction record mirrors ents-gate-rules' EDB        *)
(* relations one-to-one, as in Receive.tla.                               *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANTS
  Oids,        \* object ids in play
  RefNames,    \* meta-ref names in play
  EffectRefs,  \* the RefNames under refs/meta/effects/*
  Keys,        \* signing keys in play
  NoOid        \* model value: absent old tip / unborn ref

ASSUME EffectRefs \subseteq RefNames

Roles == {"admin", "member"}
OldOids == Oids \cup {NoOid}

VARIABLES
  refs,      \* RefNames -> OldOids: current tips (Phase 3 state)
  objects,   \* SUBSET Oids
  members,   \* SUBSET (Keys \X Roles)
  epoch,     \* Nat
  triggered, \* commits that have entered the trigger set (§6)
  queue,     \* at-least-once delivery: dedup keys <<effect, ref, new_oid>>
  results    \* result-ref writes performed so far

vars == <<refs, objects, members, epoch, triggered, queue, results>>

(* The transaction record type: ents-gate-rules' Facts, field for field. *)
Txns == [ref_update    : SUBSET (RefNames \X OldOids \X Oids),
         parent        : SUBSET (Oids \X Oids),
         signed_by     : SUBSET (Oids \X Keys),
         anchor        : SUBSET (Oids \X Oids),
         context       : SUBSET (Oids \X Oids),
         object_exists : SUBSET Oids]

\* The dedup key the doc claims yields exactly-once observable effects.
DedupKeys == EffectRefs \X RefNames \X Oids

-----------------------------------------------------------------------------
(* Actions. SKELETONS: signatures and intent only. *)

\* A gated ref advance (reuses Phase 3's GateAdmits when composed).
RefAdvance ==
  /\ TRUE  \* TODO(exercise)
  /\ UNCHANGED vars

\* A commit ENTERS the trigger set (§6 "fires once per commit that enters
\* the set") — the delete-and-repush re-entry question lives here.
TriggerEval ==
  /\ TRUE  \* TODO(exercise)
  /\ UNCHANGED vars

\* At-least-once enqueue of a dedup key.
Enqueue ==
  /\ TRUE  \* TODO(exercise)
  /\ UNCHANGED vars

\* Executor runs an effect; may crash and restart (duplicate delivery).
Execute ==
  /\ TRUE  \* TODO(exercise)
  /\ UNCHANGED vars

\* Result write-back: a gated write like any other, by an executor key.
ResultPush ==
  /\ TRUE  \* TODO(exercise)
  /\ UNCHANGED vars

-----------------------------------------------------------------------------
Init ==
  /\ refs = [r \in RefNames |-> NoOid]
  /\ objects = {}
  /\ members = {}
  /\ epoch = 0
  /\ triggered = {}
  /\ queue = {}
  /\ results = {}

Next == RefAdvance \/ TriggerEval \/ Enqueue \/ Execute \/ ResultPush

Spec == Init /\ [][Next]_vars

TypeOK ==
  /\ refs \in [RefNames -> OldOids]
  /\ objects \subseteq Oids
  /\ members \subseteq (Keys \X Roles)
  /\ epoch \in Nat
  /\ triggered \subseteq Oids
  /\ queue \subseteq DedupKeys
  /\ results \subseteq DedupKeys

=============================================================================
