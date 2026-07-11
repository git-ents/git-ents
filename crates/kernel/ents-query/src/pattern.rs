//! Refname patterns and the static footprint (`query.footprint`).

use gix::refs::FullNameRef;

use crate::error::ParseError;

/// A refname glob: literal bytes plus `*` wildcards, where each `*`
/// matches any run of characters including `/` (the same shape git
/// refspecs and the spec's own examples use — `refs/heads/*` matches
/// `refs/heads/wip/x`).
///
/// # Examples
///
/// ```
/// use ents_query::RefPattern;
///
/// let pattern = RefPattern::new("refs/heads/*").expect("valid");
/// let name: gix::refs::FullName = "refs/heads/wip/x".try_into().expect("valid");
/// assert!(pattern.matches(name.as_ref()));
///
/// let other: gix::refs::FullName = "refs/tags/v1".try_into().expect("valid");
/// assert!(!pattern.matches(other.as_ref()));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RefPattern(String);

impl RefPattern {
    /// Validate and wrap a pattern.
    ///
    /// # Errors
    ///
    /// [`ParseError::BadPattern`] when the pattern is empty or contains
    /// bytes no refname can carry (whitespace, `\`, control bytes, the
    /// query grammar's own metacharacters, `..`, or `//`).
    pub fn new(pattern: impl Into<String>) -> Result<Self, ParseError> {
        let pattern = pattern.into();
        let bad = |why: &'static str| ParseError::BadPattern {
            pattern: pattern.clone(),
            why,
        };
        if pattern.is_empty() {
            return Err(bad("empty pattern"));
        }
        if pattern
            .bytes()
            .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
        {
            return Err(bad("whitespace or control byte"));
        }
        if pattern.bytes().any(|b| b"\\()|&,?[".contains(&b)) {
            return Err(bad("byte a refname cannot carry"));
        }
        if pattern.contains("..") || pattern.contains("//") {
            return Err(bad("empty or dot-dot path segment"));
        }
        if pattern.starts_with('/') || pattern.ends_with('/') {
            return Err(bad("leading or trailing slash"));
        }
        Ok(Self(pattern))
    }

    /// The pattern text.
    ///
    /// # Examples
    ///
    /// ```
    /// use ents_query::RefPattern;
    ///
    /// assert_eq!(RefPattern::new("refs/tags/v*").expect("valid").as_str(), "refs/tags/v*");
    /// ```
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether `name` matches this pattern.
    #[must_use]
    pub fn matches(&self, name: &FullNameRef) -> bool {
        self.matches_str(&name.as_bstr().to_string())
    }

    /// [`RefPattern::matches`] over a plain string refname.
    #[must_use]
    pub(crate) fn matches_str(&self, name: &str) -> bool {
        glob_match(self.0.as_bytes(), name.as_bytes())
    }

    /// The literal bytes before the first `*` (the whole pattern when
    /// there is no wildcard).
    pub(crate) fn literal_prefix(&self) -> &str {
        self.0.split('*').next().unwrap_or(&self.0)
    }

    /// Whether some refname starting with `prefix` could match this
    /// pattern — the conservative overlap test `query.meta` needs to
    /// keep effect-written namespaces unreachable.
    pub(crate) fn may_match_with_prefix(&self, prefix: &str) -> bool {
        may_match(self.0.as_bytes(), prefix.as_bytes())
    }
}

impl std::fmt::Display for RefPattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Classic wildcard match: `*` matches any run (including `/`),
/// everything else is literal. Iterative two-pointer with backtracking.
fn glob_match(pattern: &[u8], text: &[u8]) -> bool {
    let (mut p, mut t) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while t < text.len() {
        match pattern.get(p) {
            Some(b'*') => {
                star = Some(p);
                mark = t;
                p = p.saturating_add(1);
            }
            Some(&c) if text.get(t) == Some(&c) => {
                p = p.saturating_add(1);
                t = t.saturating_add(1);
            }
            _ => match star {
                Some(s) => {
                    p = s.saturating_add(1);
                    mark = mark.saturating_add(1);
                    t = mark;
                }
                None => return false,
            },
        }
    }
    while pattern.get(p) == Some(&b'*') {
        p = p.saturating_add(1);
    }
    p == pattern.len()
}

/// Whether the pattern could match some string that starts with
/// `prefix`. `true` whenever the pattern can consume all of `prefix`
/// (whatever pattern remains can always match its own literal tail).
fn may_match(pattern: &[u8], prefix: &[u8]) -> bool {
    let Some(rest) = prefix.split_first() else {
        return true;
    };
    match pattern.split_first() {
        None => false,
        Some((b'*', tail)) => (0..=prefix.len()).any(|k| {
            prefix
                .get(k..)
                .is_some_and(|suffix| may_match(tail, suffix))
        }),
        Some((&c, tail)) => c == *rest.0 && may_match(tail, rest.1),
    }
}

/// The set of refname patterns a query depends on, extractable from its
/// syntax tree alone (`query.footprint`) — what lets `receive` map one
/// ref transition to the affected queries without re-scanning every
/// effect on every push.
///
/// # Examples
///
/// ```
/// use ents_query::Query;
///
/// let query: Query = "rev(refs/heads/main) & results(unit, pass)".parse().expect("valid");
/// let footprint = query.footprint();
///
/// let main: gix::refs::FullName = "refs/heads/main".try_into().expect("valid");
/// let result: gix::refs::FullName = "refs/meta/results/unit/abc".try_into().expect("valid");
/// let other: gix::refs::FullName = "refs/heads/dev".try_into().expect("valid");
/// assert!(footprint.matches(main.as_ref()));
/// assert!(footprint.matches(result.as_ref()));
/// assert!(!footprint.matches(other.as_ref()));
/// ```
// @relation(query.footprint, scope=file)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Footprint(Vec<RefPattern>);

impl Footprint {
    pub(crate) fn from_patterns(mut patterns: Vec<RefPattern>) -> Self {
        patterns.sort();
        patterns.dedup();
        Self(patterns)
    }

    /// The patterns, sorted and deduplicated.
    #[must_use]
    pub fn patterns(&self) -> &[RefPattern] {
        &self.0
    }

    /// Whether a transition on `name` can affect the query's set.
    #[must_use]
    pub fn matches(&self, name: &FullNameRef) -> bool {
        let text = name.as_bstr().to_string();
        self.0.iter().any(|p| p.matches_str(&text))
    }
}

#[cfg(test)]
mod tests {
    #![expect(clippy::expect_used, reason = "unit test")]

    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::exact("refs/heads/main", "refs/heads/main", true)]
    #[case::star_crosses_slashes("refs/heads/*", "refs/heads/wip/x", true)]
    #[case::mid_star("refs/tags/v*-rc", "refs/tags/v1.2-rc", true)]
    #[case::two_stars("refs/*/unit/*", "refs/meta/results/unit/abc", true)]
    #[case::suffix_must_still_match("refs/*/unit", "refs/meta/results/unit/abc", false)]
    #[case::no_match("refs/heads/*", "refs/tags/v1", false)]
    #[case::literal_shorter_than_text("refs/heads", "refs/heads/main", false)]
    // @relation(query.footprint, scope=function, role=Verifies)
    fn glob_matching_matches_git_style_star_runs(
        #[case] pattern: &str,
        #[case] name: &str,
        #[case] expected: bool,
    ) {
        let pattern = RefPattern::new(pattern).expect("valid");
        assert_eq!(pattern.matches_str(name), expected);
    }

    #[rstest]
    #[case::wildcard_reaches_into_prefix("refs/meta/*", "refs/meta/results/", true)]
    #[case::exact_inside_prefix("refs/meta/results/unit/abc", "refs/meta/results/", true)]
    #[case::disjoint("refs/meta/issues/*", "refs/meta/results/", false)]
    #[case::prefix_of_the_prefix("refs/meta/res*", "refs/meta/results/", true)]
    // @relation(query.meta, scope=function, role=Verifies)
    fn prefix_overlap_is_detected_conservatively(
        #[case] pattern: &str,
        #[case] prefix: &str,
        #[case] expected: bool,
    ) {
        let pattern = RefPattern::new(pattern).expect("valid");
        assert_eq!(pattern.may_match_with_prefix(prefix), expected);
    }
}
