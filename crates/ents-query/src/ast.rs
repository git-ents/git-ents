//! The `CommitQuery` syntax tree (`query.grammar`).

use ents_model::Status;

use crate::pattern::{Footprint, RefPattern};
use crate::rev::RevExpr;

/// The status argument of a `results()` atom: one of the closed
/// taxonomy's three values, or `any` for any recorded status
/// (`query.results`, `model.result-taxonomy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusFilter {
    /// Only `pass` results.
    Pass,
    /// Only `fail` results.
    Fail,
    /// Only `error` results.
    Error,
    /// Any recorded status — the work-set subtraction side
    /// (`query.workset`).
    Any,
}

impl StatusFilter {
    /// Whether a recorded status satisfies this filter.
    ///
    /// # Examples
    ///
    /// ```
    /// use ents_model::Status;
    /// use ents_query::StatusFilter;
    ///
    /// assert!(StatusFilter::Any.admits(Status::Fail));
    /// assert!(StatusFilter::Pass.admits(Status::Pass));
    /// assert!(!StatusFilter::Pass.admits(Status::Error));
    /// ```
    #[must_use]
    pub fn admits(&self, status: Status) -> bool {
        match self {
            Self::Any => true,
            Self::Pass => status == Status::Pass,
            Self::Fail => status == Status::Fail,
            Self::Error => status == Status::Error,
        }
    }

    pub(crate) fn parse(text: &str) -> Option<Self> {
        match text {
            "pass" => Some(Self::Pass),
            "fail" => Some(Self::Fail),
            "error" => Some(Self::Error),
            "any" => Some(Self::Any),
            _ => None,
        }
    }
}

impl std::fmt::Display for StatusFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Error => "error",
            Self::Any => "any",
        })
    }
}

/// A binary set operator (`query.set-ops`). All three share one
/// precedence level and associate left (`query.grammar`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOp {
    /// `|` — union.
    Union,
    /// `&` — intersection.
    Intersect,
    /// `-` — difference.
    Difference,
}

impl std::fmt::Display for SetOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Union => "|",
            Self::Intersect => "&",
            Self::Difference => "-",
        })
    }
}

/// A parsed `CommitQuery`: three atoms closed under union,
/// intersection, and difference (`query.grammar`), denoting a set of
/// commits as a pure function of ref state.
///
/// The enum is deliberately exhaustive and public: `query.no-extensions`
/// freezes the atom set (no content, time, or external-event terms), and
/// `query.recursion` depends on downstream-of-an-effect being visible in
/// this structure — see [`Query::results_dependencies`].
///
/// # Examples
///
/// ```
/// use ents_query::{Query, SetOp};
///
/// // The staged-pipeline idiom.
/// let query: Query = "rev(refs/heads/main) & results(unit, pass)".parse().expect("valid");
/// let Query::Op { op: SetOp::Intersect, .. } = query else { panic!("an intersection") };
/// ```
// @relation(query.grammar, query.no-extensions, scope=file)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Query {
    /// `rev(expr)` — an ordinary Git revspec or ref glob over refs
    /// outside `refs/meta/*` (`query.rev`).
    Rev(RevExpr),
    /// `results(effect, status)` — commits carrying a recorded result
    /// (`query.results`).
    Results {
        /// The effect whose results namespace is scanned.
        effect: String,
        /// Which recorded statuses count.
        status: StatusFilter,
    },
    /// `meta(glob)` — tip commits of matching author-written meta-refs
    /// (`query.meta`).
    Meta(RefPattern),
    /// A binary set operation (`query.set-ops`).
    Op {
        /// The operator.
        op: SetOp,
        /// Left operand.
        lhs: Box<Query>,
        /// Right operand.
        rhs: Box<Query>,
    },
}

impl Query {
    /// The refname patterns this query depends on, by static analysis
    /// of the syntax tree alone (`query.footprint`): a `rev` term
    /// contributes its own ref patterns, `results(effect, _)`
    /// contributes the effect's results namespace, `meta(glob)`
    /// contributes the glob itself.
    // @relation(query.footprint, scope=function)
    #[must_use]
    pub fn footprint(&self) -> Footprint {
        let mut patterns = Vec::new();
        self.collect_patterns(&mut patterns);
        Footprint::from_patterns(patterns)
    }

    fn collect_patterns(&self, out: &mut Vec<RefPattern>) {
        match self {
            Self::Rev(expr) => out.extend(expr.patterns()),
            Self::Results { effect, .. } => {
                if let Ok(pattern) = RefPattern::new(format!("refs/meta/results/{effect}/*")) {
                    out.push(pattern);
                }
            }
            Self::Meta(glob) => out.push(glob.clone()),
            Self::Op { lhs, rhs, .. } => {
                lhs.collect_patterns(out);
                rhs.collect_patterns(out);
            }
        }
    }

    /// The effects whose results this query reacts to, in syntactic
    /// order — whether a query is downstream of an effect is determined
    /// by inspecting whether `results(...)` appears in its text, never
    /// by runtime behavior (`query.recursion`).
    ///
    /// # Examples
    ///
    /// ```
    /// use ents_query::Query;
    ///
    /// let query: Query = "rev(main) & results(unit, pass) | results(integ, any)"
    ///     .parse().expect("valid");
    /// assert_eq!(query.results_dependencies(), ["unit", "integ"]);
    ///
    /// let plain: Query = "rev(main)".parse().expect("valid");
    /// assert!(plain.results_dependencies().is_empty());
    /// ```
    // @relation(query.recursion, scope=function)
    #[must_use]
    pub fn results_dependencies(&self) -> Vec<&str> {
        let mut out = Vec::new();
        self.collect_dependencies(&mut out);
        out
    }

    fn collect_dependencies<'a>(&'a self, out: &mut Vec<&'a str>) {
        match self {
            Self::Rev(_) | Self::Meta(_) => {}
            Self::Results { effect, .. } => {
                if !out.contains(&effect.as_str()) {
                    out.push(effect);
                }
            }
            Self::Op { lhs, rhs, .. } => {
                lhs.collect_dependencies(out);
                rhs.collect_dependencies(out);
            }
        }
    }
}

impl std::fmt::Display for Query {
    /// Canonical text: atoms as written, operators space-separated, a
    /// parenthesized right operand wherever left-associativity would
    /// otherwise regroup it. `parse(display(q)) == q` for every query.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rev(expr) => write!(f, "rev({})", expr.raw()),
            Self::Results { effect, status } => write!(f, "results({effect}, {status})"),
            Self::Meta(glob) => write!(f, "meta({glob})"),
            Self::Op { op, lhs, rhs } => {
                write!(f, "{lhs} {op} ")?;
                if matches!(**rhs, Self::Op { .. }) {
                    write!(f, "({rhs})")
                } else {
                    write!(f, "{rhs}")
                }
            }
        }
    }
}

impl std::str::FromStr for Query {
    type Err = crate::error::ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        crate::parse::parse(s)
    }
}
