//! `GET /`: the workbench dashboard -- `git status` for review and
//! issue tracking (`docs/web-workbench-plan.adoc`'s Phase C home page). Four
//! sections on a `.desk` grid: the working tree's changed files (a live
//! `gix` status of the repository at `state.path`), a needs-attention
//! feed of open comment threads, the open issues, and a full-width
//! History card of recent commits with their Scoped-Commits scope chips.
//! The `README` this page used to render moved to `crate::pages::files`'s
//! root listing -- the dashboard is a work surface, not a document viewer.
//!
//! The status and history reads browse the repository through `gix`'s
//! high-level `Repository` types, opened fresh per request from
//! `state.path`, exactly as [`crate::pages::files`]/[`crate::pages::commits`]
//! do (and for the same reason: browsing arbitrary repository content is
//! not the `facet-git-tree` meta-ref convention the generic pages use).
//! Every repository read here is best-effort: an unopenable repository or
//! a failed status walk degrades to an in-card note, never an error.

use std::sync::Arc;

use axum::extract::State;
use gix::bstr::ByteSlice as _;
use gix_object::{Find, Write};
use maud::{Markup, html};

use crate::error::Result;
use crate::pages::{scope_class, split_scope};
use crate::state::AppState;

/// How many commits the History card shows -- a dashboard lane, not the
/// full pager `crate::pages::commits::list` already is.
const HISTORY_LIMIT: usize = 8;

/// How many characters of a comment's or issue's first line a `.what`
/// row shows before ellipsizing.
const WHAT_LIMIT: usize = 90;

/// `GET /`.
///
/// # Errors
///
/// Propagates a ref-store or object read failure on the comment and issue
/// listings; every repository read degrades in-card instead (see this
/// module's own doc).
pub async fn show<O>(State(state): State<Arc<AppState<O>>>) -> Result<maud::Markup>
where
    O: Find + Write + Send + 'static,
{
    let changes = worktree_changes(&state);
    let (comments, _unreadable) =
        ents_forge::comment::list_all(state.refs.as_ref(), &*state.objects())?;
    let open_comments: Vec<(String, ents_forge::comment::Comment)> = comments
        .into_iter()
        .filter(|(_, comment)| comment.state == "open")
        .collect();
    let (issues, _unreadable) =
        ents_forge::issue::list_all(state.refs.as_ref(), &*state.objects())?;
    let open_issues: Vec<(String, ents_forge::Issue)> = issues
        .into_iter()
        .filter(|(_, issue)| issue.state == "open")
        .collect();
    let (history, _older) = super::commits::commit_rows(&state, None, HISTORY_LIMIT);

    let repo = super::RepoHeader::from_state(&state);
    let history_title = repo.branch.as_ref().map_or_else(
        || "History".to_owned(),
        |branch| format!("History \u{2014} {branch}"),
    );

    let attention = attention_card(&state, &open_comments, open_issues.len());
    // A bespoke `.page-header` rather than [`super::layout`]'s plain
    // title-only one: the desk's header carries a subtitle and a
    // right-aligned "local · trusted deployment" line beside the title,
    // which the shared helper has no slot for. [`super::layout_shell`] is
    // the same chrome one layer down, so this still shares every other
    // page's `.wb`/`.wb-bar` shell -- only the `.content` wrapper below is
    // grown by hand to match.
    Ok(super::layout_shell(
        &repo,
        &super::identity_label(&state),
        super::Tab::Overview,
        "Dashboard",
        html! {
            main.content {
                div.page-header {
                    div {
                        h1.page-title { "Dashboard" }
                        p.page-sub {
                            "Your morning desk \u{2014} " span.mono { "git status" } " for review and ticketing."
                        }
                    }
                    div.desk-status {
                        span.dot {}
                        "local \u{b7} trusted deployment"
                    }
                }
                div.desk {
                    (working_tree_card(&state, changes.as_deref()))
                    (attention)
                    (issues_card(&open_issues))
                }
                div.desk-wide {
                    (history_card(&history_title, &history))
                }
            }
        },
    ))
}

/// The "Working tree" card: every changed file [`worktree_changes`] found,
/// each linking into the Files browser with its open-in-editor affordance
/// ([`super::editor_open`]) beside it and its change kind chip
/// ([`kind_chip_class`]) right-aligned. The path sits in a
/// `.desk-path` container so a long path ellipsizes from the front
/// (`direction: rtl`) rather than colliding with the editor pill and kind
/// chip -- both of the latter render inside a plain wrapping `span` so
/// `ents.css`'s blanket `.card-row a { flex: 1 }` rule (aimed at the path
/// link) never reaches into them through the descendant selector. `None`
/// (the status walk itself failed) renders a note row; an empty list
/// renders a "clean" row -- either way the card itself always renders, so
/// the desk's shape is stable.
fn working_tree_card<O: Find>(
    state: &AppState<O>,
    changes: Option<&[(String, &'static str)]>,
) -> Markup {
    html! {
        section.card {
            div.card-header {
                "Working tree"
                @if let Some(changes) = changes {
                    span.entry-size { (changes.len()) " changed" }
                }
            }
            @match changes {
                None => { div.card-row.muted { "Working-tree status unavailable." } },
                Some([]) => { div.card-row.muted { "Clean \u{2014} no uncommitted changes." } },
                Some(changes) => {
                    @for (path, kind) in changes {
                        div.card-row {
                            a.desk-path href={ "/files/" (path) } { (path) }
                            span { (super::editor_open(state, path, None)) }
                            span class={ "chip " (kind_chip_class(kind)) } { (kind) }
                        }
                    }
                },
            }
        }
    }
}

/// The `.chip-*` color class for a working-tree row's change-kind label
/// (README's ChangeKindLabel component): green for a new addition, amber
/// for an ordinary modification, red for a deletion or an unresolved
/// conflict, indigo for a rename, and the neutral gray fallback for
/// anything else ([`change_kind`]'s "untracked" and "type changed" both
/// land here, matching the prototype's own `kindStyle` fallback). `.chip`
/// itself (`ents.css`) supplies the pill's shape; this only picks its
/// color.
fn kind_chip_class(kind: &str) -> &'static str {
    match kind {
        "added" => "chip-added",
        "modified" => "chip-modified",
        "deleted" | "conflict" => "chip-deleted",
        "renamed" => "chip-renamed",
        _ => "chip-untracked",
    }
}

/// The "Needs attention" card: every open comment thread, each linking to
/// its own page and naming where its anchor lands ([`comment_where`]),
/// closed by an open-issues count line when any issues are open.
fn attention_card<O: Find>(
    state: &AppState<O>,
    open_comments: &[(String, ents_forge::comment::Comment)],
    open_issue_count: usize,
) -> Markup {
    html! {
        section.card {
            div.card-header { "Needs attention" }
            @if open_comments.is_empty() && open_issue_count == 0 {
                div.card-row.muted { "Nothing waiting on you." }
            }
            @for (id, comment) in open_comments {
                a.attention-row href={ "/comments/" (id) } {
                    span.what {
                        span.lead { "open thread" }
                        " \u{2014} \u{201c}" (what_line(&comment.body)) "\u{201d}"
                    }
                    span class="where" { (comment_where(state, comment)) }
                }
            }
            @if open_issue_count > 0 {
                a.attention-row href="/issues" {
                    span.what {
                        (open_issue_count)
                        @if open_issue_count == 1 { " open issue" } @else { " open issues" }
                    }
                }
            }
        }
    }
}

/// The "Issues" card: every open issue linking to its own page, with a
/// ghost "New" button into the Issues page's own composer.
fn issues_card(open_issues: &[(String, ents_forge::Issue)]) -> Markup {
    html! {
        section.card {
            div.card-header {
                "Issues"
                a.btn.btn-ghost.btn-sm href="/issues" { "+ New" }
            }
            @if open_issues.is_empty() {
                div.card-row.muted { "No open issues." }
            }
            @for (id, issue) in open_issues {
                a.attention-row href={ "/issues/" (id) } {
                    span.what { (what_line(&issue.title)) }
                    span class="where" { "#" (ents_forge::abbreviate_id(id)) " \u{b7} " (issue.state) }
                }
            }
        }
    }
}

/// The full-width "History" card: the most recent commits, each with its
/// Scoped-Commits scope chip ([`split_scope`], [`scope_class`]) when its
/// subject carries one.
fn history_card(title: &str, rows: &[super::commits::CommitRow]) -> Markup {
    html! {
        section.card.history {
            div.card-header { (title) }
            @if rows.is_empty() {
                div.card-row.muted { "No commits yet." }
            }
            @for row in rows {
                div.card-row {
                    a href={ "/commit/" (row.oid) } { code { (row.short) } }
                    @match split_scope(&row.subject) {
                        Some((scope, rest)) => {
                            span class={ "scope " (scope_class(scope)) } { (scope) }
                            span.desk-subject { (rest) }
                        },
                        None => { span.desk-subject { (row.subject) } },
                    }
                    span.desk-when { (row.ago) }
                }
            }
        }
    }
}

/// A body's first line, ellipsized past [`WHAT_LIMIT`] characters -- what
/// a `.what` row shows of a comment or issue.
fn what_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or("");
    let mut shown: String = line.chars().take(WHAT_LIMIT).collect();
    if shown.len() < line.len() {
        shown.push('\u{2026}');
    }
    shown
}

/// Where an open comment lives, for its `.where` line: its anchor's
/// `path:line` when it carries one this build can read back, else the
/// context entity it names, else a bare "unanchored".
fn comment_where<O: Find>(state: &AppState<O>, comment: &ents_forge::comment::Comment) -> String {
    if let Some(raw) = &comment.anchor {
        let objects = state.objects();
        if let Ok(anchor) =
            facet_git_tree::deserialize::<ents_anchor::Anchor>(&raw.oid(), &*objects)
        {
            return match anchor.lines {
                Some(range) => format!("{}:{}", anchor.path, range.start),
                None => anchor.path,
            };
        }
    }
    comment
        .context
        .clone()
        .unwrap_or_else(|| "unanchored".to_owned())
}

/// Every changed path in the working tree against `HEAD` and the index --
/// `gix`'s own status walk (`gix::Repository::status`), deduplicated by
/// path (a file both staged and modified appears in the head-to-index and
/// index-to-worktree halves; the first classification wins) and sorted for
/// a stable render. `None` when the repository cannot be opened or the
/// walk cannot start at all -- [`working_tree_card`] renders a note row
/// then, never an error.
fn worktree_changes<O>(state: &AppState<O>) -> Option<Vec<(String, &'static str)>> {
    let repo = gix::open(&state.path).ok()?;
    let iter = repo
        .status(gix::progress::Discard)
        .ok()?
        .into_iter(None)
        .ok()?;
    let mut by_path: std::collections::BTreeMap<String, &'static str> =
        std::collections::BTreeMap::new();
    for item in iter.flatten() {
        let Some(kind) = change_kind(&item) else {
            continue;
        };
        by_path
            .entry(item.location().to_str_lossy().into_owned())
            .or_insert(kind);
    }
    Some(by_path.into_iter().collect())
}

/// A status item's display kind, or `None` for one that is not a change a
/// reader acts on (a stat-only refresh, an ignored entry).
fn change_kind(item: &gix::status::Item) -> Option<&'static str> {
    use gix::status::plumbing::index_as_worktree::{Change, EntryStatus};
    match item {
        gix::status::Item::TreeIndex(change) => Some(match change {
            gix::diff::index::ChangeRef::Addition { .. } => "added",
            gix::diff::index::ChangeRef::Deletion { .. } => "deleted",
            gix::diff::index::ChangeRef::Modification { .. } => "modified",
            gix::diff::index::ChangeRef::Rewrite { .. } => "renamed",
        }),
        gix::status::Item::IndexWorktree(change) => match change {
            gix::status::index_worktree::Item::Modification { status, .. } => match status {
                EntryStatus::Conflict { .. } => Some("conflict"),
                EntryStatus::Change(change) => Some(match change {
                    Change::Removed => "deleted",
                    Change::Type { .. } => "type changed",
                    Change::Modification { .. } | Change::SubmoduleModification(_) => "modified",
                }),
                EntryStatus::NeedsUpdate(_) => None,
                EntryStatus::IntentToAdd => Some("added"),
            },
            gix::status::index_worktree::Item::DirectoryContents { entry, .. } => {
                matches!(entry.status, gix::dir::entry::Status::Untracked).then_some("untracked")
            }
            gix::status::index_worktree::Item::Rewrite { .. } => Some("renamed"),
        },
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "unit test")]

    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::scoped("model: fix stale rustdoc", Some(("model", "fix stale rustdoc")))]
    #[case::hyphenated("web-ui: polish", Some(("web-ui", "polish")))]
    #[case::unscoped("Fix stale rustdoc", None)]
    #[case::uppercase_prefix("Model: fix", None)]
    #[case::no_colon("just a subject", None)]
    #[case::empty_scope(": odd", None)]
    fn split_scope_takes_only_a_lowercase_scope_prefix(
        #[case] subject: &str,
        #[case] expected: Option<(&str, &str)>,
    ) {
        assert_eq!(split_scope(subject), expected);
    }

    #[test]
    fn scope_class_is_stable_and_within_the_token_palette() {
        let class = scope_class("model");
        assert_eq!(class, scope_class("model"), "same scope, same color");
        let index: usize = class
            .strip_prefix("scope-c")
            .expect("prefixed class")
            .parse()
            .expect("numeric suffix");
        assert!(index < 6, "always one of the six --s-* token colors");
    }

    #[test]
    fn what_line_takes_the_first_line_and_ellipsizes_long_ones() {
        assert_eq!(what_line("short\nrest"), "short");
        let long = "x".repeat(200);
        let shown = what_line(&long);
        assert!(shown.chars().count() <= WHAT_LIMIT.saturating_add(1));
        assert!(shown.ends_with('\u{2026}'));
    }
}
