# Stocktaking git-ents by formal verification

A paper exercise. The repo's own claim — `docs/abstractions.adoc` opens with
"the load-bearing abstractions, stated as invariants" — is taken literally:
every invariant becomes a formal obligation, the code becomes the
implementation you check the model against, and the output is a verdict
ledger that *is* the stocktake.

Tools: **Alloy 6** for structural/relational claims (object graph,
reachability, refname binding — small-scope model finding excels at
producing the counterexample you didn't think of) and **TLA+** for the
protocol claims (receive/CAS, adoption races, effect dispatch). Paper-first:
write every model longhand before touching a checker; the checker is for
the claims you can't settle by hand.

The repo now contains its own formalization artifact:
`crates/kernel/ents-gate-rules` states seven admission invariants as
compiled Datalog (ascent). Treat it as a **fourth source** with a specific
epistemic status: it is executable and type-checked, but it *evaluates*
rules over supplied facts — it cannot search for the transaction you
didn't think of, prove inductiveness, or reason about time. Alloy/TLA+
still own those jobs; the rules crate becomes the refinement anchor the
paper models are checked against.

Rules of engagement:
- The **doc is the claim source**, the **code is ground truth**, and *you*
  (your recent design conversations) are a third, unwritten source. Every
  obligation gets a verdict from: PROVED (inductive in the model),
  FALSIFIED (counterexample, written out as a concrete Git-object diagram),
  CONDITIONAL (holds only under an assumption the doc doesn't state — write
  the assumption down; these are the real findings), or DIVERGED (doc, code,
  and/or head disagree — cite file and line).
- Scope discipline: Alloy scopes of 4–6 atoms per signature. Almost every
  bug in a system like this appears with two members, two refs, three
  commits.
- No fixing. This exercise produces the ledger, not patches.

---

## Phase 0 — Adjudicate abstraction 4 (half a day)

Two write-path models are in play:

- **Model C (doc, §4):** author-signed commits; refname a total function of
  signed content; FF-only + CAS as anti-replay; certs are transport only.
- **Model P (stated in conversation):** `push --signed`; the cert signs
  `(refname, old, new)`; certs archived reachably in a server-side op-log.

Formalize both in Alloy: signatures `Commit`, `Tree`, `Ref`, `Member`,
`Sig`, plus `Cert` in Model P. Define one predicate each system claims:

> `Auditable`: for every historical tip of every meta-ref, a verifier
> holding only the object closure of `refs/meta/*` (plus, in P, the op-log
> ref) can decide *who* placed it *there*.

Obligations:
1. Prove or refute `Auditable` in each model.
2. In P, state precisely what the op-log ref's own integrity rests on —
   what signs the op-log tip, and is that circular with the gate?
3. In C, check the doc's own caveat: "a commit signature proves authorship;
   it does not prove placement" — verify the recovery (refname recomputation)
   is *total* (Phase 2 depends on this).
4. Verdict: are C and P equivalent for `Auditable`? If not, which property
   separates them — and which one does the repo actually implement? Read
   `ents-gate/src/{verify,signature}.rs` and `ents-receive/src/receive.rs`
   and cite the lines. Update the ledger with DIVERGED entries as needed.

Standing evidence: `ents-gate-rules::unsigned_violation` requires every
introduced *commit* to be member-signed, and no `Cert` fact exists in the
schema — the executable rules encode Model C. The doc and the rules agree;
the conversational claim (`push --signed`, certs in the op-log) is the
outlier. Phase 0's burden is therefore: either produce the argument that
overturns two shipped artifacts, or record Model C as canonical and file
the op-log-cert idea as transport forensics only.

Everything downstream assumes whichever model wins. Do not proceed with
both.

## Phase 0.5 — Verify the verifier (one day)

`ents-gate-rules` is small enough to model whole. Translate each EDB
relation into an Alloy signature and each denial rule into a predicate,
then run the check the crate cannot run on itself: **search for
transactions with zero violations that break a doc invariant.**

Obligations:
1. **Refname binding is missing and unmarked.** The module docs declare
   two deliberate gaps (granularity, effect dedup) — but §4's placement
   recovery ("refname recomputes from signed content") has no rule and no
   gap marker. Confirm the concrete counterexample: an admin-signed
   parentless entity commit authored as a comment, replayed as the
   creation of `refs/meta/effects/x`, passes `genesis`, `unsigned`, and
   `effect_admin` (vacuous FF). Write it as a red test in the crate's own
   `Facts` vocabulary. Verdict for §4's binding claim: DIVERGED
   (doc claims it, rules don't check it, and per Phase 0 decide whether
   `ents-gate` proper does).
2. **Extractor contract = trusted computing base.** Every negated
   relation (`!ancestor`, `!covered`, `!member_signed`, `!object_exists`)
   is sound only under closed-world completeness of the supplied facts.
   The EDB comments say `parent` is "bounded at the old tips." For each
   negation, state the completeness assumption and its failure direction:
   under-supplied `parent` makes `ff_violation` fail *closed* (spurious
   deny — safe), but under-supplied `member`/`signed_by` also denies, while
   over-supplied `object_exists` admits dangling anchors — fail *open*.
   The table of (relation, assumption, failure direction) is a ledger
   deliverable; the fail-open rows are extractor obligations that
   currently live nowhere.
3. **Creation admits exactly one commit.** `genesis_violation` denies any
   creation whose tip has parents, and `ref_update` cannot express
   create-then-advance in one tuple — so an entity with initial history
   cannot be pushed atomically. Decide: intended (creation is always a
   bare genesis) or gap. If intended, it belongs in §2/§4 as a stated
   invariant; it currently isn't.
4. **Merge-commit signatures.** `unsigned_violation` requires *every*
   introduced commit signed, which answers Phase 3's sync question in the
   strict direction: an unsigned auto-merge from `ents-sync` would be
   denied. Check whether sync actually signs its merges; if not, the rules
   crate and the sync path are on a collision course — DIVERGED, with the
   resolution belonging to Phase 3 obligation 1.

## Phase 1 — The object-graph substrate (one day)

Alloy model of just enough Git: `Obj = Blob + Tree + Commit`, `Tree`
entries as a relation `entries: Tree -> Name -> Obj`, `parents: Commit ->
set Commit`, `reach = ^(parents + tree-closure)`. Facts: acyclicity,
content addressing as atom identity (two structurally identical trees are
the same atom — model this deliberately; dedup is load-bearing for schema
pinning).

Obligations (all from §2, §3, and the schema-pinning decision):
1. **Schema pinning:** entity tree embeds schema as a real entry ⇒ schema
   in `reach[entityTip]`. Then the negative: model the *string-OID* and
   *gitlink* variants and show reachability fails — the counterexample you
   already reasoned about informally becomes a checked fact.
2. **Anchor retention (§3):** embedded blob + context blob reachable from
   the meta-ref; anchored *commit* oid recorded as data only ⇒ show a trace
   where the anchored commit is gc'd but the anchor still projects
   (degraded). Then check the doc's parenthetical: gitlinks retain nothing.
3. **Redaction vs. retention:** §3 calls redaction "the sole deliberate
   exception." Model redaction as object withholding and check: can
   withholding one entity's blob break *another* entity's closure (shared
   blob via dedup)? This is a CONDITIONAL candidate — dedup and redaction
   pull in opposite directions.

## Phase 2 — Refname binding as a total function (one day)

The doc (§2, §4) claims: "the refname is a total function of signed
content, recomputed at verification." Totality is the whole game — one
entity kind whose refname carries information not in its signed content
reopens cross-ref replay for that kind.

In Alloy: `binding: Commit -> lone RefName` derived from content atoms.
Enumerate every namespace in `docs/spec/meta-ref.adoc` and
`ents-model/src`:
`member/*, issues/*, comments/*, effects/*, results/*, inbox/*,
self/<member>/*, account, config`.

Obligations:
1. For each namespace, write the derivation (genesis oid / natural key /
   signer composite) from the code, not from memory. Any namespace where
   you cannot write it: DIVERGED or FALSIFIED.
2. **Inbox:** an entity sits at `refs/meta/inbox/*` *before* adoption and
   at canonical *after*, with the same signed commit in ancestry. The
   function is therefore not injective over placement — check that the gate
   treats inbox as an allowed second image, and that nothing else is.
3. **`self/<member>`:** the member component derives from the signer —
   confirm in code that it derives from the *signature*, not from a tree
   field an author could set to someone else.
4. **Cross-repo replay:** is repo identity anywhere in signed content? If
   not, a validly signed genesis commit for repo A verifies in repo B.
   State whether that is a real threat in your deployment model
   (multi-tenant hosted store!) or acceptable. CONDITIONAL either way —
   write the condition.

## Phase 3 — Gate and receive as a protocol (two days)

TLA+ spec. Variables: `refs` (name → oid), `objects` (set), `members`,
`epoch`; actions: `Propose`, `GateCheck`, `CAS`, `AdoptMerge`,
`SelfMerge`. `GateCheck`'s enabling condition is now concrete: it is
`gate(facts) = {}` — transcribe the seven denial rules from
`ents-gate-rules` verbatim, plus the §5 rules the crate omits (refname
recomputation, epoch). The refinement mapping between this spec and the
crate is Phase 0.5's model reused, which is the point. Model *two* writers
and *one* hosted store minimum; add a local store (advisory gate) as a
second instance with no enforcement.

Obligations:
1. **Tip invariant is inductive:** "the tip of a meta-ref is signed by a
   member authorized for that refname" — prove it's preserved by every
   action. Pay attention to `SelfMerge` (two of your machines racing):
   *is the merge commit itself signed in the implementation?* Check
   `ents-sync/src` and `ents-receive/src/reconcile.rs`. If sync
   auto-merges without a signature, the invariant breaks at exactly the
   step the doc waves through.
2. **Adoption preserves it:** contributor commit in ancestry, adopter
   signature at tip. Then the sharper check: contributor's commit is
   itself a merge of unauthorized commits — still fine? (It should be;
   prove it, don't assume it.)
3. **Anti-replay:** doc claims parent-hash freshness ⇒ no nonce needed.
   Model a replayed genesis (parentless!) — CAS with old = ∅ against an
   existing ref fails, but against a *not-yet-created* ref? Combined with
   Phase 2 totality this should be safe; the proof forces you to state the
   exact conjunction that makes it safe.
4. **Epoch bootstrap (§5):** "the epoch-setting commit is the first gated
   tip of the config ref." Model the store from empty: is there a state
   where the gate must read the epoch from a ref whose tip is not yet
   gated? Either the bootstrap is a real fixpoint or there's an ungated
   first write — find which, cite `ents-gate/src/{config,policy}.rs`.
5. **Revocation:** revoked member's key "must never validate again" — but
   the tip invariant is checked against member state *when*? At gate time
   or by later re-verifiers against *current* member state? A member valid
   at admission and revoked later makes historical tips fail naive
   re-verification. The epoch mechanism is supposed to answer this —
   check that it actually does.

## Phase 4 — Effects (one day)

TLA+, building on Phase 3's state. Actions: `RefAdvance`,
`TriggerEval` (commit *enters* the set, §6), `Enqueue` (at-least-once),
`Execute`, `ResultPush` (a gated write like any other).

Obligations:
1. **Exactly-once observable effect from at-least-once queue:** the dedup
   key is `(effect, refname, new_oid)` — prove result-ref idempotency under
   duplicate delivery and executor crash-restart.
2. **"Fires once per commit that enters the set":** model a ref deleted
   and re-pushed to the same oid. Does the commit *re-enter* the set?
   The spec's answer defines whether triggers are monotone; if they aren't,
   the cached-query-advanceability argument (FF-only ⇒ results advance)
   has a hole. This connects to the op-log deletion-as-data design.
3. **Authorization asymmetry:** effects are admin-writable
   (authoring an effect schedules execution); results are written by
   executor member keys. Prove no sequence lets a non-admin cause
   execution of content they authored *as* an effect (the Phase 2 binding
   should close this — compose the two models and check, since this was
   the original cross-ref replay scenario).

## Phase 5 — Durability ordering (half a day, optional)

TLA+ with crash faults for the hosted deployment: `TigrisWrite`,
`PgCAS`, `Crash` at any point. Prove the invariant "no ref in Postgres
points outside the durable object set," and show the recovery obligation
if the write order were inverted. Small spec, high value — it's the
invariant your whole hosted story rests on.

## Deliverable

One ledger table: claim · source (doc §, spec file, code file:line) ·
model (Alloy/TLA+) · **encoded in ents-gate-rules?** (rule name / gap
marked / gap unmarked) · verdict · assumption-or-counterexample. Plus the
Phase 0 decision memo — which write-path model is canonical — because
every DIVERGED entry downstream resolves against it.

Rules found FALSIFIED or gap-unmarked have a natural landing spot the
exercise didn't have before: each becomes a red test plus a new denial
rule in `ents-gate-rules`, in the crate's own one-rule-at-a-time
discipline. Still out of scope for the exercise itself — record them as
ledger rows with a `rule-candidate` note.

Expected yield, honestly: the first FALSIFIED is already in hand
(cross-ref replay through the missing binding rule, Phase 0.5.1); expect
1–3 more (likeliest: unsigned sync merges colliding with
`unsigned_violation`, inbox binding edge, trigger re-entry), a handful of
CONDITIONAL that become one-line doc amendments or extractor-contract
rows, and one genuine decision (Phase 0) that no amount of model checking
makes for you — though the rules crate has already cast its vote.
