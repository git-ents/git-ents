//! The `CommitQuery` algebra (`docs/spec/query.sdoc`): three atoms —
//! `rev()`, `results()`, `meta()` — closed under union, intersection,
//! and difference, denoting a set of commits as a pure function of ref
//! state. Every effect's trigger is one of these queries; composition
//! happens by writing the query itself, never by a workflow language or
//! a runtime scheduler.
//!
//! This crate owns the grammar and parser ([`Query`]), static
//! ref-footprint extraction ([`Query::footprint`]), and evaluation
//! ([`Evaluator`]): full reconciliation-grade sets, incremental entry
//! sets bounded by generation numbers, and the work set
//! `trigger − results(self, any)`. It is deliberately separate from
//! executor and run-loop code (`arch.query-effect-split`): `receive`
//! links this crate for footprint matching on every push and must never
//! link executor code.
//!
//! # Spec coverage
//!
//! From `docs/spec/query.sdoc`:
//!
//! - `query.grammar`, `query.set-ops` — [`Query`], the parser, and
//!   [`SetOp`]; left-associative, one precedence level.
//! - `query.rev` — [`RevExpr`]; `refs/meta/*` patterns are rejected at
//!   parse time, never silently evaluated. The supported surface is
//!   exactly the rev-list-shaped subset the requirement states —
//!   refnames (short or full), `refs/` globs, full hex oids,
//!   `^negation`, and `A..B` — and every form outside it (`~n`/`^n`,
//!   `A...B`, `@{...}`, abbreviated hex) is an explicit
//!   [`ParseError::UnsupportedRev`]; growing the subset is a
//!   compatible, additive extension.
//! - `query.results` — resolution is a refname scan of the effect's
//!   results namespace; membership tests compare hex prefixes and walk
//!   no history.
//! - `query.meta` — the glob must stay under `refs/meta/*` and can
//!   never match `refs/meta/results/*` or `refs/meta/index/*`; the
//!   fanout index is not addressable by any atom.
//! - `query.no-extensions` — the atom set is closed; `time(...)`,
//!   `content(...)`, or any other name is [`ParseError::UnknownAtom`].
//! - `query.footprint` — [`Query::footprint`], from the syntax tree
//!   alone.
//! - `query.incremental`, `query.monotone` — [`Evaluator::entry_set`].
//! - `query.workset` — [`Evaluator::work_set`] (incremental) and
//!   [`Evaluator::outstanding`] (boot-time reconciliation); `self` is
//!   substituted at evaluation time and rejected in trigger text.
//! - `query.recursion` — [`Query::results_dependencies`]; downstream-of
//!   is syntax, and `rev()`/`meta()` cannot name effect-written refs,
//!   so unwritten trigger cycles are unreachable by construction.
//! - `query.rev-pattern-compat` — a bare ref glob parses as exactly
//!   `rev(<glob>)`.
//!
//! # Examples
//!
//! The staged-pipeline idiom, evaluated incrementally: integration only
//! after unit tests pass.
//!
//! ```
//! use ents_model::Status;
//! use ents_query::{Evaluator, Query, Transition};
//! use ents_testutil::{MemRefStore, ObjectStore, advance_ref, record_result};
//!
//! let refs = MemRefStore::default();
//! let objects = ObjectStore::default();
//! let commits = advance_ref(&refs, &objects, "refs/heads/main", 2, 100);
//!
//! let trigger: Query = "rev(refs/heads/main) & results(unit, pass)".parse().expect("valid");
//! let evaluator = Evaluator::new(&refs, &objects);
//!
//! // A unit result lands for the first commit: exactly that commit
//! // enters the staged trigger's set.
//! let short = commits[0].to_string()[..12].to_owned();
//! let result_tip = record_result(&refs, &objects, "unit", &short, Status::Pass, None, 300);
//! let entered = evaluator.entry_set(&trigger, &Transition {
//!     name: format!("refs/meta/results/unit/{short}").as_str().try_into().expect("valid"),
//!     old: None,
//!     new: Some(result_tip),
//! }).expect("evaluates");
//! assert_eq!(entered.into_iter().collect::<Vec<_>>(), vec![commits[0]]);
//! ```

mod ast;
mod error;
mod eval;
mod parse;
mod pattern;
mod rev;

pub use ast::{Query, SetOp, StatusFilter};
pub use error::{EvalError, EvalResult, ParseError};
pub use eval::{Evaluator, Transition};
pub use pattern::{Footprint, RefPattern};
pub use rev::RevExpr;
