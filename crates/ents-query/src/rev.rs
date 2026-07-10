//! `rev()` expressions over code refs (`query.rev`).

use gix_hash::ObjectId;

use crate::error::ParseError;
use crate::pattern::RefPattern;

/// One positive or negated term inside a `rev()` expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RevTerm {
    /// A refname, exact (`refs/heads/main`) or short (`main`, resolved
    /// through the standard gitrevisions lookup order).
    Name(String),
    /// A ref glob (`refs/heads/*`). Globs must be written in full
    /// `refs/...` form.
    Glob(RefPattern),
    /// A full hex object id.
    Oid(ObjectId),
}

impl RevTerm {
    /// The refname patterns this term can be affected by — the term's
    /// contribution to the query footprint (`query.footprint`). A short
    /// name contributes every candidate of the lookup order, since a
    /// transition on any of them can change what the name resolves to.
    pub(crate) fn patterns(&self) -> Vec<RefPattern> {
        match self {
            Self::Name(name) => dwim_candidates(name)
                .iter()
                .filter_map(|c| RefPattern::new(c.clone()).ok())
                .collect(),
            Self::Glob(pattern) => vec![pattern.clone()],
            Self::Oid(_) => Vec::new(),
        }
    }
}

/// The gitrevisions lookup order for a short refname, restricted to the
/// namespaces a ref store serves (`refs/*`). A full `refs/...` name is
/// its own single candidate.
pub(crate) fn dwim_candidates(name: &str) -> Vec<String> {
    if name.starts_with("refs/") {
        vec![name.to_owned()]
    } else {
        vec![
            format!("refs/{name}"),
            format!("refs/tags/{name}"),
            format!("refs/heads/{name}"),
            format!("refs/remotes/{name}"),
        ]
    }
}

/// A parsed `rev()` expression: whitespace-separated terms, `^`-negated
/// terms subtracted, `A..B` sugar for `^A B` — the rev-list shape of
/// `query.rev`'s "ordinary Git revspec or ref glob".
///
/// Unsupported revspec forms (`~n`/`^n` suffixes, `...`, `@{...}`,
/// `^{...}`, abbreviated hex) are an explicit [`ParseError`], never a
/// silent empty set; the supported surface is what the composition
/// idioms and `query.rev`'s own examples use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevExpr {
    include: Vec<RevTerm>,
    exclude: Vec<RevTerm>,
    raw: String,
}

impl RevExpr {
    /// Parse the text between `rev(` and `)`.
    pub(crate) fn parse(raw: &str) -> Result<Self, ParseError> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(ParseError::EmptyRev);
        }
        let mut include = Vec::new();
        let mut exclude = Vec::new();
        for token in raw.split_whitespace() {
            if let Some(negated) = token.strip_prefix('^') {
                exclude.push(parse_term(negated, token)?);
            } else if token.contains("...") {
                return Err(ParseError::UnsupportedRev {
                    token: token.to_owned(),
                });
            } else if let Some((base, tip)) = token.split_once("..") {
                if base.is_empty() || tip.is_empty() {
                    return Err(ParseError::UnsupportedRev {
                        token: token.to_owned(),
                    });
                }
                exclude.push(parse_term(base, token)?);
                include.push(parse_term(tip, token)?);
            } else {
                include.push(parse_term(token, token)?);
            }
        }
        if include.is_empty() {
            return Err(ParseError::NoPositiveRev);
        }
        Ok(Self {
            include,
            exclude,
            raw: raw.to_owned(),
        })
    }

    /// The expression text as written (trimmed), for display.
    ///
    /// # Examples
    ///
    /// ```
    /// use ents_query::Query;
    ///
    /// let query: Query = "rev(main ^release)".parse().expect("valid");
    /// let Query::Rev(expr) = query else { panic!("a rev atom") };
    /// assert_eq!(expr.raw(), "main ^release");
    /// ```
    #[must_use]
    pub fn raw(&self) -> &str {
        &self.raw
    }

    pub(crate) fn include(&self) -> &[RevTerm] {
        &self.include
    }

    pub(crate) fn exclude(&self) -> &[RevTerm] {
        &self.exclude
    }

    /// Every pattern of every term, positive and negated: a transition
    /// on a negated ref changes the denoted set too.
    pub(crate) fn patterns(&self) -> Vec<RefPattern> {
        self.include
            .iter()
            .chain(&self.exclude)
            .flat_map(RevTerm::patterns)
            .collect()
    }
}

/// Parse one term, rejecting `refs/meta/*` shapes (`query.rev`) and
/// revspec operators this evaluator does not support.
fn parse_term(term: &str, whole_token: &str) -> Result<RevTerm, ParseError> {
    let unsupported = || ParseError::UnsupportedRev {
        token: whole_token.to_owned(),
    };
    if term.is_empty() || term.contains(['~', ':', '@', '{', '}', '^']) {
        return Err(unsupported());
    }
    if term.len() == 40 && term.bytes().all(|b| b.is_ascii_hexdigit()) {
        let oid = ObjectId::from_hex(term.as_bytes()).map_err(|_e| unsupported())?;
        return Ok(RevTerm::Oid(oid));
    }
    let meta = |pattern: &str| ParseError::MetaInRev {
        pattern: pattern.to_owned(),
    };
    if term.contains('*') {
        if !term.starts_with("refs/") {
            return Err(unsupported());
        }
        let pattern = RefPattern::new(term).map_err(|_e| unsupported())?;
        // Rejected only when the pattern *names* the meta namespace; a
        // broad glob like `refs/*` is legal because `refs/meta/*` is
        // outside rev()'s domain by definition and is excluded at
        // evaluation, not silently matched (`query.rev`).
        if pattern.literal_prefix().starts_with("refs/meta") {
            return Err(meta(term));
        }
        return Ok(RevTerm::Glob(pattern));
    }
    // Charset sanity via the pattern validator (no wildcard present).
    let _validated = RefPattern::new(term).map_err(|_e| unsupported())?;
    if dwim_candidates(term)
        .iter()
        .any(|c| c.starts_with("refs/meta"))
    {
        return Err(meta(term));
    }
    Ok(RevTerm::Name(term.to_owned()))
}
