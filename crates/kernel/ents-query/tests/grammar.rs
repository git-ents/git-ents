//! Grammar, validation, footprint, and compatibility tests — rstest
//! tables, because the spec enumerates the cases.

#![expect(clippy::expect_used, clippy::panic, reason = "test code")]

use ents_query::{ParseError, Query, SetOp};
use rstest::rstest;

fn parse(input: &str) -> Query {
    input
        .parse()
        .unwrap_or_else(|e| panic!("{input:?} must parse: {e}"))
}

fn parse_err(input: &str) -> ParseError {
    input
        .parse::<Query>()
        .err()
        .unwrap_or_else(|| panic!("{input:?} must be rejected"))
}

// ---------------------------------------------------------------------
// query.grammar / query.set-ops
// ---------------------------------------------------------------------

#[rstest]
#[case::rev_atom("rev(refs/heads/main)")]
#[case::rev_range("rev(main ^release)")]
#[case::rev_dotdot("rev(release..main)")]
#[case::results_atom("results(unit, pass)")]
#[case::meta_atom("meta(refs/meta/issues/*)")]
#[case::union("rev(a) | rev(b)")]
#[case::intersection("rev(refs/heads/main) & results(unit, pass)")]
#[case::difference("rev(refs/heads/*) - rev(refs/heads/wip/*)")]
#[case::parenthesized("rev(a) & (rev(b) | rev(c))")]
#[case::whitespace_tolerant("  rev( main )   &results(unit,any)  ")]
// @relation(query.grammar, scope=function, role=Verifies)
fn well_formed_queries_parse(#[case] input: &str) {
    let _query = parse(input);
}

#[rstest]
// @relation(query.grammar, query.set-ops, scope=function, role=Verifies)
fn operators_are_left_associative_at_one_precedence_level() {
    // a | b & c  parses as  (a | b) & c — never as  a | (b & c).
    let query = parse("rev(a) | rev(b) & rev(c)");
    let Query::Op {
        op: SetOp::Intersect,
        lhs,
        rhs,
    } = query
    else {
        panic!("top-level operator must be the rightmost one");
    };
    assert!(matches!(
        *lhs,
        Query::Op {
            op: SetOp::Union,
            ..
        }
    ));
    assert!(matches!(*rhs, Query::Rev(_)));
}

#[rstest]
// @relation(query.set-ops, scope=function, role=Verifies)
fn parentheses_regroup_evaluation_order() {
    let grouped = parse("rev(a) | (rev(b) & rev(c))");
    let Query::Op {
        op: SetOp::Union,
        rhs,
        ..
    } = grouped
    else {
        panic!("parentheses must override left-to-right order");
    };
    assert!(matches!(
        *rhs,
        Query::Op {
            op: SetOp::Intersect,
            ..
        }
    ));
}

#[rstest]
#[case::atom("rev(refs/heads/main)")]
#[case::rev_with_negation("rev(main ^release)")]
#[case::results("results(unit, pass)")]
#[case::meta("meta(refs/meta/issues/*)")]
#[case::left_chain("rev(a) | rev(b) & rev(c) - rev(d)")]
#[case::grouped("rev(a) - (rev(b) | rev(c))")]
#[case::bare_glob("refs/heads/*")]
// @relation(query.grammar, scope=function, role=Verifies)
fn display_round_trips_through_the_parser(#[case] input: &str) {
    let query = parse(input);
    let rendered = query.to_string();
    assert_eq!(parse(&rendered), query, "display: {rendered:?}");
}

#[rstest]
#[case::empty("")]
#[case::bare_operator("| rev(a)")]
#[case::dangling_operator("rev(a) |")]
#[case::unbalanced_paren("(rev(a)")]
#[case::unbalanced_atom("rev(main")]
#[case::trailing_garbage("rev(a) rev(b)")]
#[case::missing_status("results(unit)")]
#[case::empty_rev("rev()")]
#[case::only_negations("rev(^main)")]
// @relation(query.grammar, scope=function, role=Verifies)
fn malformed_queries_are_rejected(#[case] input: &str) {
    let _err = parse_err(input);
}

// ---------------------------------------------------------------------
// query.no-extensions — the atom set is closed.
// ---------------------------------------------------------------------

#[rstest]
#[case::time_atom("time(5m)")]
#[case::cron_atom("cron(hourly) & rev(main)")]
#[case::content_atom("content(Cargo.toml)")]
#[case::path_atom("rev(main) & path(src/*)")]
#[case::webhook_atom("webhook(deploy)")]
// @relation(query.no-extensions, scope=function, role=Verifies)
fn content_time_and_external_event_atoms_do_not_exist(#[case] input: &str) {
    assert!(matches!(parse_err(input), ParseError::UnknownAtom { .. }));
}

// ---------------------------------------------------------------------
// query.rev — refs/meta/* is outside rev()'s domain.
// ---------------------------------------------------------------------

#[rstest]
#[case::exact_meta_ref("rev(refs/meta/config)")]
#[case::meta_glob("rev(refs/meta/issues/*)")]
#[case::meta_short_name("rev(meta/issues)")]
#[case::negated_meta("rev(main ^refs/meta/config)")]
// @relation(query.rev, scope=function, role=Verifies)
fn rev_naming_a_meta_pattern_is_malformed_not_empty(#[case] input: &str) {
    assert!(matches!(parse_err(input), ParseError::MetaInRev { .. }));
}

#[rstest]
#[case::triple_dot("rev(a...b)")]
#[case::tilde_suffix("rev(main~3)")]
#[case::caret_suffix("rev(main^2)")]
#[case::reflog("rev(main@{1})")]
#[case::peel("rev(v1.0^{commit})")]
// @relation(query.rev, scope=function, role=Verifies)
fn unsupported_revspec_forms_error_explicitly(#[case] input: &str) {
    assert!(matches!(
        parse_err(input),
        ParseError::UnsupportedRev { .. }
    ));
}

// ---------------------------------------------------------------------
// query.meta — effect-written namespaces are unreachable.
// ---------------------------------------------------------------------

#[rstest]
#[case::results_glob("meta(refs/meta/results/*)")]
#[case::results_exact("meta(refs/meta/results/unit/abc)")]
#[case::index_glob("meta(refs/meta/index/*)")]
#[case::broad_meta_glob("meta(refs/meta/*)")]
#[case::sneaky_prefix("meta(refs/meta/res*)")]
// @relation(query.meta, query.recursion, scope=function, role=Verifies)
fn meta_cannot_match_effect_written_namespaces(#[case] input: &str) {
    assert!(matches!(
        parse_err(input),
        ParseError::MetaGlobEffectWritten { .. }
    ));
}

#[rstest]
#[case::heads_glob("meta(refs/heads/*)")]
#[case::short_glob("meta(issues/*)")]
// @relation(query.meta, scope=function, role=Verifies)
fn meta_glob_must_stay_under_refs_meta(#[case] input: &str) {
    assert!(matches!(
        parse_err(input),
        ParseError::MetaGlobOutside { .. }
    ));
}

// ---------------------------------------------------------------------
// query.workset — `self` is notation, not a keyword.
// ---------------------------------------------------------------------

#[rstest]
// @relation(query.workset, scope=function, role=Verifies)
fn results_self_cannot_be_written_in_a_trigger() {
    assert_eq!(parse_err("results(self, any)"), ParseError::SelfKeyword);
}

#[rstest]
#[case::bad_status("results(unit, maybe)")]
#[case::empty_effect("results(, pass)")]
#[case::effect_with_slash("results(a/b, pass)")]
#[case::effect_with_star("results(a*, pass)")]
// @relation(query.results, scope=function, role=Verifies)
fn results_arguments_are_validated(#[case] input: &str) {
    let err = parse_err(input);
    assert!(
        matches!(
            err,
            ParseError::BadStatus { .. } | ParseError::BadEffectName { .. }
        ),
        "got {err:?}"
    );
}

// ---------------------------------------------------------------------
// query.rev-pattern-compat — bare glob is the degenerate rev() query.
// ---------------------------------------------------------------------

#[rstest]
#[case::heads_glob("refs/heads/*")]
#[case::tags_glob("refs/tags/v*")]
#[case::exact_ref("refs/heads/main")]
#[case::short_name("main")]
// @relation(query.rev-pattern-compat, scope=function, role=Verifies)
fn a_bare_ref_glob_means_exactly_rev_of_that_glob(#[case] glob: &str) {
    assert_eq!(parse(glob), parse(&format!("rev({glob})")));
}

#[rstest]
// @relation(query.rev-pattern-compat, query.rev, scope=function, role=Verifies)
fn a_bare_meta_glob_is_still_rejected() {
    assert!(matches!(
        parse_err("refs/meta/*"),
        ParseError::MetaInRev { .. } | ParseError::UnexpectedEnd
    ));
}

// ---------------------------------------------------------------------
// query.footprint — static extraction from the syntax tree alone.
// ---------------------------------------------------------------------

#[rstest]
#[case::exact_rev("rev(refs/heads/main)", &["refs/heads/main"])]
#[case::results("results(unit, pass)", &["refs/meta/results/unit/*"])]
#[case::meta("meta(refs/meta/issues/*)", &["refs/meta/issues/*"])]
#[case::rev_negation_contributes(
    "rev(refs/heads/main ^refs/heads/release)",
    &["refs/heads/main", "refs/heads/release"]
)]
#[case::composite(
    "rev(refs/heads/main) & results(unit, pass)",
    &["refs/heads/main", "refs/meta/results/unit/*"]
)]
#[case::short_name_contributes_the_lookup_order(
    "rev(main)",
    &["refs/main", "refs/tags/main", "refs/heads/main", "refs/remotes/main"]
)]
// @relation(query.footprint, scope=function, role=Verifies)
fn footprints_come_from_the_syntax_tree_alone(#[case] input: &str, #[case] expected: &[&str]) {
    let footprint = parse(input).footprint();
    let mut got: Vec<&str> = footprint.patterns().iter().map(|p| p.as_str()).collect();
    let mut expected: Vec<&str> = expected.to_vec();
    got.sort_unstable();
    expected.sort_unstable();
    assert_eq!(got, expected);
}

#[rstest]
#[case::affected("refs/meta/results/unit/abc123", true)]
#[case::other_effect("refs/meta/results/integ/abc123", false)]
#[case::trigger_ref("refs/heads/main", true)]
#[case::unrelated("refs/heads/dev", false)]
// @relation(query.footprint, scope=function, role=Verifies)
fn footprint_maps_a_ref_transition_to_affected_queries(
    #[case] refname: &str,
    #[case] affected: bool,
) {
    let query = parse("rev(refs/heads/main) & results(unit, pass)");
    let name: gix::refs::FullName = refname.try_into().expect("valid");
    assert_eq!(query.footprint().matches(name.as_ref()), affected);
}

// ---------------------------------------------------------------------
// query.recursion — downstream-of is syntax.
// ---------------------------------------------------------------------

#[rstest]
#[case::not_downstream("rev(refs/heads/main)", &[])]
#[case::single("rev(main) & results(unit, pass)", &["unit"])]
#[case::fan_in("results(unit, pass) & results(integ, pass)", &["unit", "integ"])]
#[case::deduplicated("results(unit, pass) | results(unit, fail)", &["unit"])]
// @relation(query.recursion, scope=function, role=Verifies)
fn downstream_of_an_effect_is_visible_in_the_text(#[case] input: &str, #[case] expected: &[&str]) {
    assert_eq!(parse(input).results_dependencies(), expected);
}
