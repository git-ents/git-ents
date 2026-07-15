//! Query evaluation: full sets, incremental entry sets, and work sets
//! (`query.incremental`, `query.monotone`, `query.workset`).
//!
//! # Evaluation model
//!
//! The evaluator is pure over the read half of the ref store plus
//! gitoxide's object-find seam. A [`Transition`] carries both sides of
//! one ref's movement; every internal read goes through a state view
//! that overrides that one ref, so the same evaluator answers "was this
//! commit in the set before?" and "is it now?" against a single store.
//!
//! # Incrementality (`query.incremental`)
//!
//! An entry set is computed from the transition frontier: candidate
//! commits come only from the symmetric difference of the affected
//! atoms (a generation-bounded paint-down walk between the old and new
//! tips for `rev`, the one tested commit for `results`, the two tips
//! for `meta`), then each candidate is membership-tested against the
//! new and old states — reachability tests pruned by generation
//! numbers, results tests by refname scan (`query.results`), never a
//! walk of full history. Generation numbers are computed lazily and
//! memoized per evaluator; a persistent commit-graph file is a later
//! optimization with the same bound.
//!
//! # Monotonicity (`query.monotone`)
//!
//! Entry sets are additions only. A shrinking ref (force-push, branch
//! deletion) produces an empty or smaller entry set; nothing is ever
//! retracted, because the only durable record — a written result —
//! lives in immutable history, and the work set subtracts it by
//! refname scan.

use std::cell::RefCell;
use std::collections::{BTreeSet, BinaryHeap, HashMap, HashSet};
use std::rc::Rc;

use ents_model::{ResultRecord, Status};
use gix::refs::FullName;
use gix_hash::ObjectId;
use gix_object::{CommitRef, Find, Kind};
use gix_ref_store::RefStoreRead;

use crate::ast::{Query, StatusFilter};
use crate::error::{EvalError, EvalResult};
use crate::rev::{RevExpr, RevTerm, dwim_candidates};

/// One ref's movement, as `receive` observes it: the refname, the tip
/// before, and the tip after (`None` on either side for creation and
/// deletion).
///
/// # Examples
///
/// ```
/// use ents_query::Transition;
///
/// let t = Transition {
///     name: "refs/heads/main".try_into().expect("valid"),
///     old: None,
///     new: Some(gix_hash::ObjectId::null(gix_hash::Kind::Sha1)),
/// };
/// assert!(t.old.is_none());
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    /// The ref that moved.
    pub name: FullName,
    /// Its tip before the transition (`None`: the ref did not exist).
    pub old: Option<ObjectId>,
    /// Its tip after the transition (`None`: the ref was deleted).
    pub new: Option<ObjectId>,
}

/// Which side of a [`Transition`] a read observes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Old,
    New,
}

/// Cached structure of one commit: parents and generation number
/// (1 + the maximum parent generation; roots are generation 1).
#[derive(Debug)]
struct CommitInfo {
    parents: Vec<ObjectId>,
    generation: u64,
}

/// A `CommitQuery` evaluator over one ref store and one object store.
///
/// Caches commit structure (parents, generation numbers) and result
/// statuses across calls, so reusing one evaluator across many
/// transitions amortizes history walks — the shape a long-lived
/// `receive` process has.
///
/// # Examples
///
/// ```
/// use ents_query::{Evaluator, Query};
/// use ents_testutil::{MemRefStore, ObjectStore, advance_ref};
///
/// let refs = MemRefStore::default();
/// let objects = ObjectStore::default();
/// let commits = advance_ref(&refs, &objects, "refs/heads/main", 2, 100);
///
/// let query: Query = "rev(refs/heads/main)".parse().expect("valid");
/// let evaluator = Evaluator::new(&refs, &objects);
/// let set = evaluator.eval(&query).expect("evaluates");
/// assert_eq!(set.len(), 2);
/// assert!(set.contains(&commits[0]) && set.contains(&commits[1]));
/// ```
pub struct Evaluator<'a> {
    refs: &'a dyn RefStoreRead,
    objects: &'a dyn Find,
    info: RefCell<HashMap<ObjectId, Rc<CommitInfo>>>,
    status: RefCell<HashMap<ObjectId, Status>>,
}

impl std::fmt::Debug for Evaluator<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Evaluator")
            .field("cached_commits", &self.info.borrow().len())
            .finish_non_exhaustive()
    }
}

impl<'a> Evaluator<'a> {
    /// Build an evaluator over `refs` and `objects`.
    pub fn new(refs: &'a dyn RefStoreRead, objects: &'a dyn Find) -> Self {
        Self {
            refs,
            objects,
            info: RefCell::new(HashMap::new()),
            status: RefCell::new(HashMap::new()),
        }
    }

    // -- public API ----------------------------------------------------

    /// The full set `query` denotes against current ref state — the
    /// reconciliation-grade evaluation (boot-time work-set scans);
    /// steady-state consumers use [`Evaluator::entry_set`].
    pub fn eval(&self, query: &Query) -> EvalResult<BTreeSet<ObjectId>> {
        self.eval_side(query, None, Side::New)
    }

    /// Whether `oid` is in `query`'s set against current ref state.
    ///
    /// Reachability tests are pruned by generation numbers; results
    /// tests are refname scans (`query.results`).
    pub fn contains(&self, query: &Query, oid: ObjectId) -> EvalResult<bool> {
        self.contains_side(query, oid, None, Side::New)
    }

    /// The commits that *enter* `query`'s set under `transition`,
    /// computed incrementally from that frontier (`query.incremental`)
    /// with entry-only semantics (`query.monotone`): an effect fires
    /// once per commit in this set; commits leaving the set appear
    /// nowhere and retract nothing.
    ///
    /// # Examples
    ///
    /// ```
    /// use ents_query::{Evaluator, Query, Transition};
    /// use ents_testutil::{MemRefStore, ObjectStore, advance_ref};
    ///
    /// let refs = MemRefStore::default();
    /// let objects = ObjectStore::default();
    /// let first = advance_ref(&refs, &objects, "refs/heads/main", 1, 100);
    /// let second = advance_ref(&refs, &objects, "refs/heads/main", 1, 200);
    ///
    /// let query: Query = "rev(refs/heads/main)".parse().expect("valid");
    /// let evaluator = Evaluator::new(&refs, &objects);
    /// let entered = evaluator.entry_set(&query, &Transition {
    ///     name: "refs/heads/main".try_into().expect("valid"),
    ///     old: Some(first[0]),
    ///     new: Some(second[0]),
    /// }).expect("evaluates");
    /// assert_eq!(entered.into_iter().collect::<Vec<_>>(), vec![second[0]]);
    /// ```
    // @relation(query.incremental, query.monotone, scope=function)
    pub fn entry_set(
        &self,
        query: &Query,
        transition: &Transition,
    ) -> EvalResult<BTreeSet<ObjectId>> {
        if !query.footprint().matches(transition.name.as_ref()) {
            return Ok(BTreeSet::new());
        }
        let mut candidates = HashSet::new();
        self.collect_candidates(query, transition, &mut candidates)?;
        let mut entered = BTreeSet::new();
        for oid in candidates {
            if self.contains_side(query, oid, Some(transition), Side::New)?
                && !self.contains_side(query, oid, Some(transition), Side::Old)?
            {
                entered.insert(oid);
            }
        }
        Ok(entered)
    }

    /// The work set for `effect` under `transition`:
    /// `trigger − results(self, any)` with `self` substituted here, at
    /// evaluation time — the entry set of the trigger minus every
    /// commit already carrying any recorded result for `effect`, by
    /// refname scan of the effect's results namespace, never a walk of
    /// history (`query.workset`).
    ///
    /// The effect's own results ref is the sole materialization marker:
    /// there is no pipeline state anywhere else to consult.
    // @relation(query.workset, scope=function)
    pub fn work_set(
        &self,
        effect: &str,
        trigger: &Query,
        transition: &Transition,
    ) -> EvalResult<BTreeSet<ObjectId>> {
        let entered = self.entry_set(trigger, transition)?;
        self.subtract_results(effect, entered, Some(transition))
    }

    /// The full outstanding set for `effect` against current ref
    /// state: `eval(trigger) − results(self, any)` — the boot-time
    /// reconciliation form of [`Evaluator::work_set`], from which the
    /// obligation queue is reconstructible (`query.workset`).
    // @relation(query.workset, scope=function)
    pub fn outstanding(&self, effect: &str, trigger: &Query) -> EvalResult<BTreeSet<ObjectId>> {
        let full = self.eval(trigger)?;
        self.subtract_results(effect, full, None)
    }

    fn subtract_results(
        &self,
        effect: &str,
        set: BTreeSet<ObjectId>,
        transition: Option<&Transition>,
    ) -> EvalResult<BTreeSet<ObjectId>> {
        let recorded = self.results_index(effect, StatusFilter::Any, transition, Side::New)?;
        Ok(set
            .into_iter()
            .filter(|oid| !prefix_matches(&recorded, *oid))
            .collect())
    }

    // -- state views ---------------------------------------------------

    /// Resolve `name` in the given state: the transition overrides its
    /// own ref on both sides, so the underlying store may hold either
    /// the pre- or post-transition value.
    fn resolve(
        &self,
        name: &str,
        transition: Option<&Transition>,
        side: Side,
    ) -> EvalResult<Option<ObjectId>> {
        if let Some(t) = transition
            && t.name.as_bstr() == name
        {
            return Ok(match side {
                Side::Old => t.old,
                Side::New => t.new,
            });
        }
        let Ok(full) = FullName::try_from(name.to_owned()) else {
            return Ok(None);
        };
        Ok(self.refs.get(full.as_ref())?)
    }

    /// All refs under `prefix` in the given state, transition applied.
    fn iter_refs(
        &self,
        prefix: &str,
        transition: Option<&Transition>,
        side: Side,
    ) -> EvalResult<Vec<(String, ObjectId)>> {
        let mut out = Vec::new();
        for entry in self.refs.iter_prefix(prefix)? {
            let (name, oid) = entry?;
            out.push((name.as_bstr().to_string(), oid));
        }
        if let Some(t) = transition {
            let name = t.name.as_bstr().to_string();
            if name.starts_with(prefix) {
                out.retain(|(n, _)| *n != name);
                let value = match side {
                    Side::Old => t.old,
                    Side::New => t.new,
                };
                if let Some(oid) = value {
                    out.push((name, oid));
                }
            }
        }
        Ok(out)
    }

    // -- atoms ---------------------------------------------------------

    /// The positive and negative tip sets of a rev expression in one
    /// state. Short names resolve through the gitrevisions lookup
    /// order; an unresolved name contributes nothing (a trigger over a
    /// not-yet-created branch denotes the empty set); globs expand over
    /// `refs/*` minus `refs/meta/*`, which is outside `rev()`'s domain
    /// by definition (`query.rev`).
    // @relation(query.rev, scope=function)
    fn rev_tips(
        &self,
        expr: &RevExpr,
        transition: Option<&Transition>,
        side: Side,
    ) -> EvalResult<(Vec<ObjectId>, Vec<ObjectId>)> {
        let resolve_terms = |terms: &[RevTerm]| -> EvalResult<Vec<ObjectId>> {
            let mut tips = Vec::new();
            for term in terms {
                match term {
                    RevTerm::Oid(oid) => tips.push(*oid),
                    RevTerm::Name(name) => {
                        for candidate in dwim_candidates(name) {
                            if let Some(oid) = self.resolve(&candidate, transition, side)? {
                                tips.push(oid);
                                break;
                            }
                        }
                    }
                    RevTerm::Glob(pattern) => {
                        for (name, oid) in self.iter_refs("refs/", transition, side)? {
                            if !name.starts_with("refs/meta/") && pattern.matches_str(&name) {
                                tips.push(oid);
                            }
                        }
                    }
                }
            }
            Ok(tips)
        };
        Ok((
            resolve_terms(expr.include())?,
            resolve_terms(expr.exclude())?,
        ))
    }

    /// The recorded-result index for one effect in one state: the
    /// short-oid refname segments (hex prefixes of tested commits)
    /// whose recorded status satisfies `filter` — a scan of refname
    /// patterns under the effect's results namespace, never a walk of
    /// commit history (`query.results`).
    // @relation(query.results, scope=function)
    fn results_index(
        &self,
        effect: &str,
        filter: StatusFilter,
        transition: Option<&Transition>,
        side: Side,
    ) -> EvalResult<Vec<String>> {
        let prefix = format!("refs/meta/results/{effect}/");
        let mut shorts = Vec::new();
        for (name, tip) in self.iter_refs(&prefix, transition, side)? {
            let Some(short) = name.strip_prefix(&prefix) else {
                continue;
            };
            if short.contains('/') || short.is_empty() {
                continue;
            }
            if filter == StatusFilter::Any || filter.admits(self.result_status(&name, tip)?) {
                shorts.push(short.to_ascii_lowercase());
            }
        }
        Ok(shorts)
    }

    /// The recorded status behind one results ref tip, cached: the tip
    /// commit's tree deserialized as a [`ResultRecord`], of which the
    /// status is one field (`model.result-identity`: the tree also carries
    /// the effect and judged commit, so a signed status means something
    /// with the refname stripped away).
    fn result_status(&self, name: &str, tip: ObjectId) -> EvalResult<Status> {
        if let Some(status) = self.status.borrow().get(&tip) {
            return Ok(*status);
        }
        let tree = self.commit_tree(tip)?;
        let record: ResultRecord =
            facet_git_tree::deserialize(&tree, self.objects).map_err(|source| {
                EvalError::Status {
                    name: name.to_owned(),
                    source,
                }
            })?;
        self.status.borrow_mut().insert(tip, record.status);
        Ok(record.status)
    }

    /// The tip commits of every author-written meta-ref matching the
    /// glob (`query.meta`); the parser already guarantees the glob
    /// cannot match an effect-written namespace.
    // @relation(query.meta, scope=function)
    fn meta_tips(
        &self,
        pattern: &crate::pattern::RefPattern,
        transition: Option<&Transition>,
        side: Side,
    ) -> EvalResult<Vec<ObjectId>> {
        let mut tips = Vec::new();
        for (name, oid) in self.iter_refs("refs/meta/", transition, side)? {
            if pattern.matches_str(&name) {
                tips.push(oid);
            }
        }
        Ok(tips)
    }

    // -- full evaluation ------------------------------------------------

    fn eval_side(
        &self,
        query: &Query,
        transition: Option<&Transition>,
        side: Side,
    ) -> EvalResult<BTreeSet<ObjectId>> {
        match query {
            Query::Rev(expr) => {
                let (include, exclude) = self.rev_tips(expr, transition, side)?;
                let reached = self.reachable(&include)?;
                let excluded = self.reachable(&exclude)?;
                Ok(reached.difference(&excluded).copied().collect())
            }
            Query::Results { effect, status } => {
                let shorts = self.results_index(effect, *status, transition, side)?;
                let universe = self.universe(transition, side)?;
                Ok(universe
                    .into_iter()
                    .filter(|oid| prefix_matches(&shorts, *oid))
                    .collect())
            }
            Query::Meta(pattern) => Ok(self
                .meta_tips(pattern, transition, side)?
                .into_iter()
                .collect()),
            // @relation(query.set-ops, scope=function)
            Query::Op { op, lhs, rhs } => {
                let l = self.eval_side(lhs, transition, side)?;
                let r = self.eval_side(rhs, transition, side)?;
                Ok(match op {
                    crate::ast::SetOp::Union => l.union(&r).copied().collect(),
                    crate::ast::SetOp::Intersect => l.intersection(&r).copied().collect(),
                    crate::ast::SetOp::Difference => l.difference(&r).copied().collect(),
                })
            }
        }
    }

    /// Every commit reachable from any ref in the given state — the
    /// resolution universe for standalone `results()` evaluation, where
    /// the refname scan yields hex prefixes that must name real
    /// commits. Only full (reconciliation-grade) evaluation pays this;
    /// membership tests compare prefixes directly.
    fn universe(
        &self,
        transition: Option<&Transition>,
        side: Side,
    ) -> EvalResult<HashSet<ObjectId>> {
        let tips: Vec<ObjectId> = self
            .iter_refs("refs/", transition, side)?
            .into_iter()
            .map(|(_, oid)| oid)
            .collect();
        self.reachable(&tips)
    }

    // -- membership -----------------------------------------------------

    fn contains_side(
        &self,
        query: &Query,
        oid: ObjectId,
        transition: Option<&Transition>,
        side: Side,
    ) -> EvalResult<bool> {
        match query {
            Query::Rev(expr) => {
                let (include, exclude) = self.rev_tips(expr, transition, side)?;
                Ok(self.reaches(&include, oid)? && !self.reaches(&exclude, oid)?)
            }
            Query::Results { effect, status } => {
                let shorts = self.results_index(effect, *status, transition, side)?;
                Ok(prefix_matches(&shorts, oid))
            }
            Query::Meta(pattern) => Ok(self.meta_tips(pattern, transition, side)?.contains(&oid)),
            Query::Op { op, lhs, rhs } => {
                let l = self.contains_side(lhs, oid, transition, side)?;
                let r = self.contains_side(rhs, oid, transition, side)?;
                Ok(match op {
                    crate::ast::SetOp::Union => l || r,
                    crate::ast::SetOp::Intersect => l && r,
                    crate::ast::SetOp::Difference => l && !r,
                })
            }
        }
    }

    // -- candidates -----------------------------------------------------

    /// Commits whose membership in some atom of `query` can have
    /// changed under `transition` — the frontier the entry set is
    /// filtered from. Everything else provably kept its membership in
    /// every atom, so it cannot have entered the composite.
    fn collect_candidates(
        &self,
        query: &Query,
        transition: &Transition,
        out: &mut HashSet<ObjectId>,
    ) -> EvalResult<()> {
        let moved = transition.name.as_bstr().to_string();
        match query {
            Query::Rev(expr) => {
                if expr
                    .patterns()
                    .iter()
                    .any(|pattern| pattern.matches_str(&moved))
                {
                    let old: Vec<_> = transition.old.into_iter().collect();
                    let new: Vec<_> = transition.new.into_iter().collect();
                    out.extend(self.ahead_of(&new, &old)?);
                    out.extend(self.ahead_of(&old, &new)?);
                }
            }
            Query::Results { effect, .. } => {
                let prefix = format!("refs/meta/results/{effect}/");
                if let Some(short) = moved.strip_prefix(&prefix)
                    && !short.contains('/')
                    && let Some(tested) = self.resolve_short(short, transition)?
                {
                    out.insert(tested);
                }
            }
            Query::Meta(pattern) => {
                if pattern.matches_str(&moved) {
                    out.extend(transition.old);
                    out.extend(transition.new);
                }
            }
            Query::Op { lhs, rhs, .. } => {
                self.collect_candidates(lhs, transition, out)?;
                self.collect_candidates(rhs, transition, out)?;
            }
        }
        Ok(())
    }

    /// Resolve a results refname's short-oid segment to the full tested
    /// commit id, searching commits reachable from the post-transition
    /// ref state. A prefix that resolves to nothing reachable yields no
    /// candidate: an unreachable commit is not an actionable entry.
    fn resolve_short(&self, short: &str, transition: &Transition) -> EvalResult<Option<ObjectId>> {
        let short = short.to_ascii_lowercase();
        let universe = self.universe(Some(transition), Side::New)?;
        Ok(universe
            .into_iter()
            .find(|oid| oid.to_string().starts_with(&short)))
    }

    // -- commit walks ----------------------------------------------------

    /// Structure of `oid`, cached: parents and generation number,
    /// resolved iteratively so a long first-parent chain cannot
    /// overflow the stack.
    fn commit_info(&self, oid: ObjectId) -> EvalResult<Rc<CommitInfo>> {
        if let Some(info) = self.info.borrow().get(&oid) {
            return Ok(Rc::clone(info));
        }
        let mut pending: HashMap<ObjectId, Vec<ObjectId>> = HashMap::new();
        let mut stack = vec![oid];
        while let Some(&top) = stack.last() {
            if self.info.borrow().contains_key(&top) {
                stack.pop();
                continue;
            }
            let parents = match pending.get(&top) {
                Some(parents) => parents.clone(),
                None => {
                    let parents = self.read_parents(top)?;
                    pending.insert(top, parents.clone());
                    parents
                }
            };
            let unresolved: Vec<ObjectId> = {
                let cache = self.info.borrow();
                parents
                    .iter()
                    .filter(|p| !cache.contains_key(*p))
                    .copied()
                    .collect()
            };
            if unresolved.is_empty() {
                let generation = {
                    let cache = self.info.borrow();
                    parents
                        .iter()
                        .filter_map(|p| cache.get(p).map(|i| i.generation))
                        .max()
                        .unwrap_or(0)
                        .saturating_add(1)
                };
                self.info.borrow_mut().insert(
                    top,
                    Rc::new(CommitInfo {
                        parents,
                        generation,
                    }),
                );
                stack.pop();
            } else {
                stack.extend(unresolved);
            }
        }
        let cache = self.info.borrow();
        cache
            .get(&oid)
            .map(Rc::clone)
            .ok_or(EvalError::Missing { oid })
    }

    fn read_parents(&self, oid: ObjectId) -> EvalResult<Vec<ObjectId>> {
        let mut buf = Vec::new();
        let data = self
            .objects
            .try_find(&oid, &mut buf)
            .map_err(|source| EvalError::Object { oid, source })?
            .ok_or(EvalError::Missing { oid })?;
        if data.kind != Kind::Commit {
            return Err(EvalError::Decode {
                oid,
                detail: format!("expected a commit, found a {}", data.kind),
            });
        }
        let commit =
            CommitRef::from_bytes(data.data, oid.kind()).map_err(|e| EvalError::Decode {
                oid,
                detail: e.to_string(),
            })?;
        Ok(commit.parents().collect())
    }

    /// The tree of the commit at `oid`.
    fn commit_tree(&self, oid: ObjectId) -> EvalResult<ObjectId> {
        let mut buf = Vec::new();
        let data = self
            .objects
            .try_find(&oid, &mut buf)
            .map_err(|source| EvalError::Object { oid, source })?
            .ok_or(EvalError::Missing { oid })?;
        if data.kind != Kind::Commit {
            return Err(EvalError::Decode {
                oid,
                detail: format!("expected a commit, found a {}", data.kind),
            });
        }
        let commit =
            CommitRef::from_bytes(data.data, oid.kind()).map_err(|e| EvalError::Decode {
                oid,
                detail: e.to_string(),
            })?;
        Ok(commit.tree())
    }

    /// Everything reachable from `tips` (inclusive) — full-walk
    /// reachability, used by reconciliation-grade evaluation only.
    fn reachable(&self, tips: &[ObjectId]) -> EvalResult<HashSet<ObjectId>> {
        let mut seen = HashSet::new();
        let mut queue: Vec<ObjectId> = tips.to_vec();
        while let Some(oid) = queue.pop() {
            if !seen.insert(oid) {
                continue;
            }
            queue.extend(self.commit_info(oid)?.parents.iter().copied());
        }
        Ok(seen)
    }

    /// Whether any tip reaches `target` by parent edges, pruning every
    /// path once its generation drops below `target`'s — the
    /// generation-number bound of `query.incremental`.
    fn reaches(&self, tips: &[ObjectId], target: ObjectId) -> EvalResult<bool> {
        if tips.contains(&target) {
            return Ok(true);
        }
        if tips.is_empty() {
            return Ok(false);
        }
        let floor = self.commit_info(target)?.generation;
        let mut heap = BinaryHeap::new();
        let mut seen = HashSet::new();
        for &tip in tips {
            let info = self.commit_info(tip)?;
            if info.generation >= floor && seen.insert(tip) {
                heap.push((info.generation, tip));
            }
        }
        while let Some((_, oid)) = heap.pop() {
            if oid == target {
                return Ok(true);
            }
            for parent in self.commit_info(oid)?.parents.iter().copied() {
                if seen.insert(parent) {
                    let generation = self.commit_info(parent)?.generation;
                    if generation >= floor {
                        heap.push((generation, parent));
                    }
                }
            }
        }
        Ok(false)
    }

    /// Commits reachable from `new_tips` but not from `old_tips` — the
    /// transition frontier, walked in descending generation order so
    /// old-side paint stops at the frontier's own depth instead of
    /// walking to the roots.
    fn ahead_of(&self, new_tips: &[ObjectId], old_tips: &[ObjectId]) -> EvalResult<Vec<ObjectId>> {
        const NEW: u8 = 1;
        const OLD: u8 = 2;
        if new_tips.is_empty() {
            return Ok(Vec::new());
        }
        let mut flags: HashMap<ObjectId, u8> = HashMap::new();
        let mut heap: BinaryHeap<(u64, ObjectId)> = BinaryHeap::new();
        let mut queued: HashSet<ObjectId> = HashSet::new();
        let mut new_only_queued = 0usize;

        let push = |oid: ObjectId,
                    flag: u8,
                    flags: &mut HashMap<ObjectId, u8>,
                    heap: &mut BinaryHeap<(u64, ObjectId)>,
                    queued: &mut HashSet<ObjectId>,
                    new_only: &mut usize|
         -> EvalResult<()> {
            let entry = flags.entry(oid).or_insert(0);
            let before = *entry;
            *entry |= flag;
            let after = *entry;
            if queued.insert(oid) {
                heap.push((self.commit_info(oid)?.generation, oid));
                if after == NEW {
                    *new_only = new_only.saturating_add(1);
                }
            } else if before == NEW && after != NEW {
                *new_only = new_only.saturating_sub(1);
            }
            Ok(())
        };

        for &tip in old_tips {
            push(
                tip,
                OLD,
                &mut flags,
                &mut heap,
                &mut queued,
                &mut new_only_queued,
            )?;
        }
        for &tip in new_tips {
            push(
                tip,
                NEW,
                &mut flags,
                &mut heap,
                &mut queued,
                &mut new_only_queued,
            )?;
        }

        let mut ahead = Vec::new();
        while let Some((_, oid)) = heap.pop() {
            // Descending generation order means every commit that could
            // paint `oid` has already been processed, so its flag is
            // final here.
            let flag = flags.get(&oid).copied().unwrap_or(0);
            queued.remove(&oid);
            if flag == NEW {
                new_only_queued = new_only_queued.saturating_sub(1);
                ahead.push(oid);
            }
            for parent in self.commit_info(oid)?.parents.clone() {
                push(
                    parent,
                    flag,
                    &mut flags,
                    &mut heap,
                    &mut queued,
                    &mut new_only_queued,
                )?;
            }
            if new_only_queued == 0 {
                // Nothing purely-new remains queued: everything deeper
                // is reachable from the old tips too, so the frontier
                // is complete — this is the generation bound.
                break;
            }
        }
        Ok(ahead)
    }
}

/// Whether any short-oid hex prefix in `shorts` prefixes `oid`.
fn prefix_matches(shorts: &[String], oid: ObjectId) -> bool {
    let hex = oid.to_string();
    shorts.iter().any(|short| hex.starts_with(short.as_str()))
}
