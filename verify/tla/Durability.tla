--------------------------- MODULE Durability ---------------------------
(***************************************************************************)
(* Phase 5 — durability ordering (verify/exercise.md, "Phase 5").          *)
(*                                                                         *)
(* SKELETON. Variables and action signatures only; every body is a        *)
(* TODO(exercise) stub that changes no state. This module checks NOTHING  *)
(* until the human exercise fills it in.                                  *)
(*                                                                         *)
(* Deployment note: the exercise document frames this phase around a      *)
(* hosted Tigris-object-store + Postgres-CAS split. The project currently *)
(* deploys neither — serving is plain git http-backend over one           *)
(* filesystem — so the actions here are named for the general shape       *)
(* (object write, ref CAS, crash), and the Tigris/Pg instantiation is     *)
(* deferred until such a deployment exists. The invariant under study is  *)
(* unchanged: no ref points outside the durable object set.               *)
(*                                                                         *)
(* Vocabulary: object ids and refnames as in the other modules, matching  *)
(* ents-gate-rules' EDB vocabulary.                                       *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANTS
  Oids,      \* object ids in play
  RefNames,  \* meta-ref names in play
  NoOid      \* model value: unborn ref

OldOids == Oids \cup {NoOid}

VARIABLES
  durable,   \* SUBSET Oids: objects durably written
  refstore   \* RefNames -> OldOids: the ref store's current tips

vars == <<durable, refstore>>

-----------------------------------------------------------------------------
(* Actions. SKELETONS: signatures and intent only. *)

\* An object (or pack) reaches durable storage.
ObjectWrite ==
  /\ TRUE  \* TODO(exercise)
  /\ UNCHANGED vars

\* The ref store compare-and-swaps a tip.
RefCAS ==
  /\ TRUE  \* TODO(exercise): the write-order question lives here
  /\ UNCHANGED vars

\* Crash at any point; recovery obligations follow from what survives.
Crash ==
  /\ TRUE  \* TODO(exercise)
  /\ UNCHANGED vars

-----------------------------------------------------------------------------
Init ==
  /\ durable = {}
  /\ refstore = [r \in RefNames |-> NoOid]

Next == ObjectWrite \/ RefCAS \/ Crash

Spec == Init /\ [][Next]_vars

TypeOK ==
  /\ durable \subseteq Oids
  /\ refstore \in [RefNames -> OldOids]

\* The invariant this phase exists to prove. STATED here so the ledger
\* row has a formal object to point at; NOT proved — the actions above
\* are stubs, so checking it today says nothing. TODO(exercise).
RefsPointDurable ==
  \A r \in RefNames : refstore[r] # NoOid => refstore[r] \in durable

=============================================================================
