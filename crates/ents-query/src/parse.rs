//! The `CommitQuery` parser (`query.grammar`), including the
//! bare-glob compatibility rule (`query.rev-pattern-compat`) and every
//! write-time rejection `effect.validation` cites: unknown atoms
//! (`query.no-extensions`), `refs/meta/*` inside `rev()` (`query.rev`),
//! and effect-written namespaces inside `meta()` (`query.meta`).

use crate::ast::{Query, SetOp, StatusFilter};
use crate::error::ParseError;
use crate::pattern::RefPattern;
use crate::rev::RevExpr;

/// The namespaces `meta()` must never be able to match (`query.meta`):
/// recorded results are reachable only through `results(...)`, and the
/// fanout index is not addressable by any query atom at all.
const EFFECT_WRITTEN: [&str; 2] = ["refs/meta/results/", "refs/meta/index/"];

/// Parse a `CommitQuery`.
///
/// A bare ref glob is accepted wherever a query is expected, meaning
/// exactly `rev(<glob>)` (`query.rev-pattern-compat`) — so a
/// `RefPattern` predating `CommitQuery` keeps denoting the same set.
// @relation(query.grammar, query.rev-pattern-compat, scope=function)
pub(crate) fn parse(input: &str) -> Result<Query, ParseError> {
    let mut parser = Parser { input, pos: 0 };
    match parser.parse_query_complete() {
        Ok(query) => Ok(query),
        Err(err) => {
            // The degenerate form: the whole (trimmed) input is one
            // glob/refname token with none of the grammar's own
            // structure in it. Its validation errors (a refs/meta/*
            // glob, above all) are the ones worth surfacing.
            let token = input.trim();
            if token.is_empty()
                || token
                    .bytes()
                    .any(|b| b.is_ascii_whitespace() || b"()|&,".contains(&b))
            {
                return Err(err);
            }
            RevExpr::parse(token).map(Query::Rev)
        }
    }
}

struct Parser<'a> {
    input: &'a str,
    pos: usize,
}

impl Parser<'_> {
    fn parse_query_complete(&mut self) -> Result<Query, ParseError> {
        let query = self.parse_query()?;
        self.skip_ws();
        if self.pos < self.input.len() {
            return Err(ParseError::Trailing {
                rest: self.rest().to_owned(),
            });
        }
        Ok(query)
    }

    /// `query ::= term (("|" | "&" | "-") term)*` — left-associative,
    /// one precedence level (`query.grammar`).
    fn parse_query(&mut self) -> Result<Query, ParseError> {
        let mut lhs = self.parse_term()?;
        loop {
            self.skip_ws();
            let op = match self.peek() {
                Some(b'|') => SetOp::Union,
                Some(b'&') => SetOp::Intersect,
                Some(b'-') => SetOp::Difference,
                _ => return Ok(lhs),
            };
            self.pos = self.pos.saturating_add(1);
            let rhs = self.parse_term()?;
            lhs = Query::Op {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
    }

    fn parse_term(&mut self) -> Result<Query, ParseError> {
        self.skip_ws();
        match self.peek() {
            None => Err(ParseError::UnexpectedEnd),
            Some(b'(') => {
                let opened_at = self.pos;
                self.pos = self.pos.saturating_add(1);
                let inner = self.parse_query()?;
                self.skip_ws();
                if self.peek() == Some(b')') {
                    self.pos = self.pos.saturating_add(1);
                    Ok(inner)
                } else {
                    Err(ParseError::Unbalanced { at: opened_at })
                }
            }
            Some(_) => self.parse_atom(),
        }
    }

    fn parse_atom(&mut self) -> Result<Query, ParseError> {
        let name = self.take_while(|b| b.is_ascii_alphanumeric() || b == b'_');
        if name.is_empty() {
            return Err(ParseError::Expected {
                expected: "an atom (rev, results, or meta)",
                at: self.pos,
            });
        }
        let name = name.to_owned();
        self.skip_ws();
        if self.peek() != Some(b'(') {
            return Err(ParseError::Expected {
                expected: "'(' after the atom name",
                at: self.pos,
            });
        }
        let opened_at = self.pos;
        self.pos = self.pos.saturating_add(1);
        let args = self.take_balanced(opened_at)?.to_owned();
        match name.as_str() {
            "rev" => Ok(Query::Rev(RevExpr::parse(&args)?)),
            "results" => parse_results(&args, self.pos),
            "meta" => parse_meta(&args),
            // The closed atom set: a content, time, or external-event
            // term is an unknown atom, permanently
            // (`query.no-extensions`).
            _ => Err(ParseError::UnknownAtom { name }),
        }
    }

    /// Consume up to the `)` matching the `(` at `opened_at` (exclusive)
    /// and step past it, returning the enclosed text.
    fn take_balanced(&mut self, opened_at: usize) -> Result<&str, ParseError> {
        let start = self.pos;
        let mut depth = 1usize;
        while let Some(b) = self.peek() {
            match b {
                b'(' => depth = depth.saturating_add(1),
                b')' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        let inner = self.input.get(start..self.pos).unwrap_or_default();
                        self.pos = self.pos.saturating_add(1);
                        return Ok(inner);
                    }
                }
                _ => {}
            }
            self.pos = self.pos.saturating_add(1);
        }
        Err(ParseError::Unbalanced { at: opened_at })
    }

    fn peek(&self) -> Option<u8> {
        self.input.as_bytes().get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while self.peek().is_some_and(|b| b.is_ascii_whitespace()) {
            self.pos = self.pos.saturating_add(1);
        }
    }

    fn take_while(&mut self, keep: impl Fn(u8) -> bool) -> &str {
        let start = self.pos;
        while self.peek().is_some_and(&keep) {
            self.pos = self.pos.saturating_add(1);
        }
        self.input.get(start..self.pos).unwrap_or_default()
    }

    fn rest(&self) -> &str {
        self.input.get(self.pos..).unwrap_or_default()
    }
}

/// `results(effect, status)` — two arguments, an effect name that is a
/// valid single ref-path segment (`effect.definition`) and never the
/// reserved `self` (`query.workset`), and a status from the closed
/// taxonomy.
fn parse_results(args: &str, at: usize) -> Result<Query, ParseError> {
    let Some((effect, status)) = args.split_once(',') else {
        return Err(ParseError::Expected {
            expected: "results(effect, status)",
            at,
        });
    };
    let effect = effect.trim();
    let status = status.trim();
    if effect == "self" {
        return Err(ParseError::SelfKeyword);
    }
    if effect.is_empty() || effect.contains('/') || !valid_ref_segment(effect) {
        return Err(ParseError::BadEffectName {
            got: effect.to_owned(),
        });
    }
    let status = StatusFilter::parse(status).ok_or_else(|| ParseError::BadStatus {
        got: status.to_owned(),
    })?;
    Ok(Query::Results {
        effect: effect.to_owned(),
        status,
    })
}

/// A single segment is valid exactly when gitoxide accepts it inside a
/// full refname — no parallel validation rules (`arch` sibling rule:
/// gitoxide types are the primitives).
fn valid_ref_segment(segment: &str) -> bool {
    gix::refs::FullName::try_from(format!("refs/meta/results/{segment}/x")).is_ok()
}

/// `meta(glob)` — must stay under `refs/meta/*` and must not be able to
/// match an effect-written namespace (`query.meta`).
fn parse_meta(args: &str) -> Result<Query, ParseError> {
    let glob = args.trim();
    let pattern = RefPattern::new(glob)?;
    if !glob.starts_with("refs/meta/") {
        return Err(ParseError::MetaGlobOutside {
            glob: glob.to_owned(),
        });
    }
    for namespace in EFFECT_WRITTEN {
        if pattern.may_match_with_prefix(namespace) {
            return Err(ParseError::MetaGlobEffectWritten {
                glob: glob.to_owned(),
                namespace,
            });
        }
    }
    Ok(Query::Meta(pattern))
}
