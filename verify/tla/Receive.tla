---------------------------- MODULE Receive ----------------------------
(***************************************************************************)
(* Phase 3 — gate and receive as a protocol (verify/exercise.md,           *)
(* "Phase 3").                                                             *)
(*                                                                         *)
(* SKELETON. Variables and action signatures only. The one definition     *)
(* with real content is GateAdmits: the seven denial rules of             *)
(* crates/kernel/ents-gate-rules/src/lib.rs transcribed mechanically —    *)
(* that transcription is the refinement anchor (Phase 0.5's model         *)
(* reused). Every action body is a TODO(exercise) stub that changes no    *)
(* state; this module checks NOTHING about the protocol until the human   *)
(* exercise fills the actions in.                                         *)
(*                                                                         *)
(* Discharges, once filled in: docs/abstractions.adoc §5 (tip invariant,  *)
(* adoption, revocation), §4 (anti-replay); docs/spec/receive.adoc;       *)
(* docs/spec/gate.adoc epoch bootstrap.                                   *)
(*                                                                         *)
(* Vocabulary: a transaction is a record whose fields mirror the EDB      *)
(* relations of ents-gate-rules one-to-one: ref_update, parent,           *)
(* signed_by, anchor, context, object_exists — with member supplied from  *)
(* the protocol-state variable `members`, exactly as the crate's          *)
(* extractor would supply it.                                             *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANTS
  Oids,        \* object ids in play
  RefNames,    \* meta-ref names in play
  EffectRefs,  \* the RefNames under refs/meta/effects/*
  Keys,        \* signing keys in play
  NoOid        \* model value: absent old tip (entity creation)

ASSUME EffectRefs \subseteq RefNames

Roles == {"admin", "member"}
OldOids == Oids \cup {NoOid}

VARIABLES
  refs,      \* RefNames -> OldOids: current tips (NoOid = unborn)
  objects,   \* SUBSET Oids: the store's object set
  members,   \* SUBSET (Keys \X Roles): enrolled keys and provenance
  epoch,     \* Nat: the config ref's epoch (§5) — placeholder until Phase 3
  proposed   \* transactions in flight, each a record over the EDB fields

vars == <<refs, objects, members, epoch, proposed>>

(* The transaction record type: ents-gate-rules' Facts, field for field. *)
Txns == [ref_update    : SUBSET (RefNames \X OldOids \X Oids),
         parent        : SUBSET (Oids \X Oids),
         signed_by     : SUBSET (Oids \X Keys),
         anchor        : SUBSET (Oids \X Oids),
         context       : SUBSET (Oids \X Oids),
         object_exists : SUBSET Oids]

-----------------------------------------------------------------------------
(* IDB relations of ents-gate-rules, transcribed. *)

\* ancestor: transitive ancestry over the transaction's parent edges.
TC(R) ==
  LET N == {p[1] : p \in R} \cup {p[2] : p \in R}
  IN { ab \in N \X N :
        \E n \in 1..Cardinality(N) :
          \E f \in [1..(n+1) -> N] :
            /\ f[1] = ab[1]
            /\ f[n+1] = ab[2]
            /\ \A i \in 1..n : <<f[i], f[i+1]>> \in R }

Ancestors(t, c) == {a \in Oids : <<c, a>> \in TC(t.parent)}

HasParent(t, c) == \E p \in Oids : <<c, p>> \in t.parent

\* covered: commits already covered by a ref's old tip.
Covered(t, old) == IF old = NoOid THEN {} ELSE {old} \cup Ancestors(t, old)

\* introduced: the new tip and its ancestors, minus everything covered.
Introduced(t, old, new) == ({new} \cup Ancestors(t, new)) \ Covered(t, old)

MemberSigned(t, mem, c) ==
  \E k \in Keys : /\ <<c, k>> \in t.signed_by
                  /\ \E r \in Roles : <<k, r>> \in mem

AdminSigned(t, mem, c) ==
  \E k \in Keys : /\ <<c, k>> \in t.signed_by
                  /\ <<k, "admin">> \in mem

-----------------------------------------------------------------------------
(* The seven denial rules, same names as the crate. *)

\* Fast-forward-only: the new tip must descend from the old tip.
FfViolation(t) ==
  \E u \in t.ref_update : /\ u[2] # NoOid
                          /\ u[2] # u[3]
                          /\ u[2] \notin Ancestors(t, u[3])

\* Creation must point at a parentless genesis commit.
GenesisViolation(t) ==
  \E u \in t.ref_update : /\ u[2] = NoOid
                          /\ HasParent(t, u[3])

\* One entity, one root: past genesis, no second parentless commit.
SecondRootViolation(t) ==
  \E u \in t.ref_update :
    /\ u[2] # NoOid
    /\ \E c \in Introduced(t, u[2], u[3]) : ~HasParent(t, c)

\* Every introduced commit carries an enrolled member's signature.
UnsignedViolation(t, mem) ==
  \E u \in t.ref_update :
    \E c \in Introduced(t, u[2], u[3]) : ~MemberSigned(t, mem, c)

\* An anchored blob must resolve.
DanglingAnchorViolation(t) ==
  \E u \in t.ref_update :
    \E c \in Introduced(t, u[2], u[3]) :
      \E b \in Oids : /\ <<c, b>> \in t.anchor
                      /\ b \notin t.object_exists

\* The paired context blob must resolve too.
DanglingContextViolation(t) ==
  \E u \in t.ref_update :
    \E c \in Introduced(t, u[2], u[3]) :
      \E b \in Oids : /\ <<c, b>> \in t.context
                      /\ b \notin t.object_exists

\* A write to refs/meta/effects/* must be admin-signed.
EffectAdminViolation(t, mem) ==
  \E u \in t.ref_update :
    /\ u[1] \in EffectRefs
    /\ \E c \in Introduced(t, u[2], u[3]) : ~AdminSigned(t, mem, c)

(* gate(facts) = {}: the conjunction that admits a transaction.          *)
GateAdmits(t, mem) ==
  /\ ~FfViolation(t)
  /\ ~GenesisViolation(t)
  /\ ~SecondRootViolation(t)
  /\ ~UnsignedViolation(t, mem)
  /\ ~DanglingAnchorViolation(t)
  /\ ~DanglingContextViolation(t)
  /\ ~EffectAdminViolation(t, mem)
  \* TODO(exercise): refname recomputation from signed content — the §4
  \* binding rule ents-gate-rules omits (ledger row: DIVERGED). Phase 3
  \* decides whether ents-gate proper enforces it and adds it here.
  \* TODO(exercise): epoch rule (§5) — also omitted from the crate.

-----------------------------------------------------------------------------
(* Actions. SKELETONS: signatures and intent only; every body leaves the  *)
(* state unchanged. TODO(exercise) throughout — nothing here is checked.  *)

\* A writer proposes a transaction (two writers minimum in the model).
Propose ==
  /\ TRUE  \* TODO(exercise): choose t \in Txns, add to proposed
  /\ UNCHANGED vars

\* The gate evaluates a proposed transaction; enabling condition is the
\* seven-rule conjunction above.
GateCheck ==
  /\ \E t \in proposed : GateAdmits(t, members)
  /\ TRUE  \* TODO(exercise): mark t admitted for CAS
  /\ UNCHANGED vars

\* Compare-and-swap on the ref tip (anti-replay, §4).
CAS ==
  /\ TRUE  \* TODO(exercise): refs' = [refs EXCEPT ...] guarded on old tip
  /\ UNCHANGED vars

\* Adoption: contributor commit in ancestry, adopter signature at tip.
AdoptMerge ==
  /\ TRUE  \* TODO(exercise)
  /\ UNCHANGED vars

\* Two of one member's machines racing (is the merge commit signed?).
SelfMerge ==
  /\ TRUE  \* TODO(exercise): check ents-sync/ents-receive reconcile path
  /\ UNCHANGED vars

-----------------------------------------------------------------------------
Init ==
  /\ refs = [r \in RefNames |-> NoOid]
  /\ objects = {}
  /\ members = {}
  /\ epoch = 0
  /\ proposed = {}

Next == Propose \/ GateCheck \/ CAS \/ AdoptMerge \/ SelfMerge

Spec == Init /\ [][Next]_vars

TypeOK ==
  /\ refs \in [RefNames -> OldOids]
  /\ objects \subseteq Oids
  /\ members \subseteq (Keys \X Roles)
  /\ epoch \in Nat
  /\ \A t \in proposed : t \in Txns

=============================================================================
