//! Parse-time and evaluation-time error types.
//!
//! [`ParseError`] is a *validation verdict on author input*: an effect
//! definition carrying a trigger that fails to parse must be rejected
//! before it is stored (`effect.validation`), so every variant explains
//! what the author wrote wrong. [`EvalError`] is infrastructure — a
//! store read failed mid-evaluation — and never a statement about the
//! query's meaning.

use gix_hash::ObjectId;

/// Everything that makes a `CommitQuery` malformed (`query.grammar`,
/// `query.rev`, `query.meta`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ParseError {
    /// The input ended where a term was required.
    #[error("unexpected end of query; expected a term")]
    UnexpectedEnd,

    /// An atom other than `rev`, `results`, or `meta` was named. The
    /// grammar deliberately has no content, time, or external-event
    /// atoms (`query.no-extensions`).
    #[error(
        "unknown atom {name:?}: the grammar has exactly rev(), results(), and meta() \
         (query.no-extensions)"
    )]
    UnknownAtom {
        /// The atom name as written.
        name: String,
    },

    /// Structurally expected input was missing at `at` (byte offset).
    #[error("expected {expected} at byte {at}")]
    Expected {
        /// What the parser needed.
        expected: &'static str,
        /// Byte offset into the query text.
        at: usize,
    },

    /// A parenthesis never closed.
    #[error("unbalanced parenthesis opened at byte {at}")]
    Unbalanced {
        /// Byte offset of the unmatched `(`.
        at: usize,
    },

    /// The query parsed, but input remained.
    #[error("trailing input after query: {rest:?}")]
    Trailing {
        /// The unconsumed tail.
        rest: String,
    },

    /// `results()` was given a status outside the closed taxonomy.
    #[error("results() status must be pass, fail, error, or any; got {got:?}")]
    BadStatus {
        /// The status as written.
        got: String,
    },

    /// `results()` was given an effect name that is not a valid single
    /// ref-path segment (`effect.definition`).
    #[error("effect name {got:?} is not a valid ref-path segment")]
    BadEffectName {
        /// The effect name as written.
        got: String,
    },

    /// `results(self, ...)` was written in a trigger. `self` is
    /// notation substituted at evaluation time (`query.workset`), never
    /// a keyword an author may write.
    #[error(
        "`self` is substituted at evaluation time (query.workset); it cannot be written \
         in a trigger"
    )]
    SelfKeyword,

    /// A `rev()` expression named a `refs/meta/*` pattern, which is
    /// outside `rev()`'s domain by definition (`query.rev`).
    #[error("rev() must not name a refs/meta/* pattern; got {pattern:?} (query.rev)")]
    MetaInRev {
        /// The offending pattern.
        pattern: String,
    },

    /// A `meta()` glob does not start with `refs/meta/`, so it could
    /// never match an author-written meta-ref.
    #[error("meta() glob must start with refs/meta/; got {glob:?} (query.meta)")]
    MetaGlobOutside {
        /// The glob as written.
        glob: String,
    },

    /// A `meta()` glob could match an effect-written namespace —
    /// `refs/meta/results/*` or `refs/meta/index/*` — which must be
    /// unreachable from `meta()` (`query.meta`, `query.recursion`).
    #[error(
        "meta() glob {glob:?} could match the effect-written namespace {namespace} \
         (query.meta)"
    )]
    MetaGlobEffectWritten {
        /// The glob as written.
        glob: String,
        /// The forbidden namespace prefix it could match.
        namespace: &'static str,
    },

    /// `rev()` was given an empty expression.
    #[error("empty rev() expression")]
    EmptyRev,

    /// `rev()` had only `^`-negated terms; at least one positive term
    /// is required to denote a non-trivial set.
    #[error("rev() needs at least one positive (non-negated) term")]
    NoPositiveRev,

    /// A revspec form this evaluator does not support yet. Unsupported
    /// forms are an explicit error, never a silent empty set.
    #[error(
        "unsupported revspec form {token:?}: supported forms are refnames, refs/ globs, \
         full hex object ids, ^negation, and A..B ranges"
    )]
    UnsupportedRev {
        /// The unsupported token.
        token: String,
    },

    /// A ref pattern contained bytes a refname can never contain.
    #[error("invalid ref pattern {pattern:?}: {why}")]
    BadPattern {
        /// The pattern as written.
        pattern: String,
        /// What is wrong with it.
        why: &'static str,
    },
}

/// Everything that can prevent evaluation from completing — always
/// infrastructure, never a property of the query.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EvalError {
    /// The ref store's read half failed.
    #[error("ref store read failed: {0}")]
    Refs(#[from] gix_ref_store::Error),

    /// The object store failed while looking up `oid`.
    #[error("object lookup failed for {oid}: {source}")]
    Object {
        /// The object being looked up.
        oid: ObjectId,
        /// The underlying object-store error.
        #[source]
        source: gix_object::find::Error,
    },

    /// `oid` was referenced (by a ref tip or a parent edge) but is not
    /// in the object store.
    #[error("object {oid} is missing from the object store")]
    Missing {
        /// The absent object.
        oid: ObjectId,
    },

    /// `oid` exists but could not be decoded as a commit.
    #[error("object {oid} could not be decoded: {detail}")]
    Decode {
        /// The undecodable object.
        oid: ObjectId,
        /// What failed, human-readable.
        detail: String,
    },

    /// A results ref's tip tree did not deserialize as a recorded
    /// [`ents_model::Status`]. Evaluation fails rather than guessing a
    /// status (`model.result-taxonomy` is a closed taxonomy).
    #[error("results ref {name} has an unreadable status tree: {source}")]
    Status {
        /// The results refname.
        name: String,
        /// The typed-tree deserialization error.
        #[source]
        source: facet_git_tree::Error,
    },
}

/// The `Result` alias for evaluation operations.
pub type EvalResult<T> = std::result::Result<T, EvalError>;
