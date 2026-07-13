//! `GET /search`: `super::layout`'s nav search form's target -- a plain
//! request-time substring scan over the served repository, deliberately
//! with no index and no new state (a design decision this crate settled
//! on rather than relitigated here): every request re-walks the `HEAD`
//! tree and the meta-ref listings the pages that already own them use.
//! Renders with no tab active at all (`super::Tab::None`), like
//! [`super::account`], since it is reached from the nav search form
//! rather than any tab.

use std::sync::Arc;

use axum::extract::{Query, State};
use ents_kiln::toolchain;
use gix::bstr::ByteSlice as _;
use gix_object::{Find, Write};
use maud::{Markup, html};
use serde::Deserialize;

use crate::error::Result;
use crate::state::AppState;

/// The largest number of matches shown per result group -- past it, a
/// "more matches not shown" note replaces the rest rather than rendering
/// an unbounded page.
const MAX_RESULTS: usize = 100;

/// The query parameters `GET /search` accepts.
#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    /// The search term. Empty (the default, and what a bare `GET /search`
    /// carries) renders the same friendly blankslate as a query with no
    /// matches.
    #[serde(default)]
    q: String,
}

/// `GET /search`: grouped, case-insensitive substring matches -- file
/// paths from the `HEAD` tree (linking into `crate::pages::files`) and
/// meta entity names (member usernames, effect names, toolchain names,
/// linking into their own show pages) -- or a blankslate on an empty
/// query or no matches.
///
/// # Errors
///
/// Propagates a ref-store read failure.
pub async fn show<O>(
    State(state): State<Arc<AppState<O>>>,
    Query(params): Query<SearchQuery>,
) -> Result<Markup>
where
    O: Find + Write + Send + 'static,
{
    let query = params.q.trim().to_owned();
    let (files, files_more) = search_files(&state, &query);
    let (members, members_more) = meta_names(&state, "refs/meta/member/", &query)?;
    let (effects, effects_more) = meta_names(&state, "refs/meta/effects/", &query)?;
    let (toolchains, toolchains_more) = search_toolchains(&state, &query)?;

    let any_matches =
        !files.is_empty() || !members.is_empty() || !effects.is_empty() || !toolchains.is_empty();

    Ok(super::layout(
        &super::RepoHeader::from_state(&state),
        &super::identity_label(&state),
        super::Tab::None,
        "search",
        html! {
            @if !any_matches {
                (blankslate(&query))
            } @else {
                (result_group("files", &files, files_more, |path| format!("/files/{path}")))
                (result_group("members", &members, members_more, |id| format!("/members/{id}")))
                (result_group("effects", &effects, effects_more, |id| format!("/effects/{id}")))
                (result_group(
                    "toolchains",
                    &toolchains,
                    toolchains_more,
                    |id| format!("/toolchains/{id}"),
                ))
            }
        },
    ))
}

/// Truncate `items` to [`MAX_RESULTS`], reporting whether anything was cut.
fn cap(mut items: Vec<String>) -> (Vec<String>, bool) {
    if items.len() > MAX_RESULTS {
        items.truncate(MAX_RESULTS);
        (items, true)
    } else {
        (items, false)
    }
}

/// File paths under the `HEAD` tree whose path contains `query`
/// (case-insensitive), capped via [`cap`]. Empty on an empty `query` --
/// no walk is attempted at all, matching every other group. Best-effort:
/// an unopenable repository or unborn `HEAD` degrade to no file matches
/// rather than an error, exactly as `crate::pages::files`/`crate::pages::dashboard`
/// degrade the same reads.
fn search_files<O>(state: &AppState<O>, query: &str) -> (Vec<String>, bool) {
    if query.is_empty() {
        return (Vec::new(), false);
    }
    let Ok(repo) = gix::open(&state.path) else {
        return (Vec::new(), false);
    };
    let Ok(tree) = repo.head_tree() else {
        return (Vec::new(), false);
    };
    let mut paths = Vec::new();
    collect_paths(&repo, &tree, "", &mut paths);
    let needle = query.to_lowercase();
    cap(paths
        .into_iter()
        .filter(|path| path.to_lowercase().contains(&needle))
        .collect())
}

/// Recurse `tree`, pushing every blob's full slash-joined path (relative
/// to the `HEAD` root) onto `out` -- the same walk
/// `crate::pages::dashboard::collect_blobs` performs for the language
/// breakdown, here collecting paths instead of `(name, oid)` pairs.
/// Subtree reads that fail are skipped rather than propagated, matching
/// that same best-effort stance.
fn collect_paths(
    repo: &gix::Repository,
    tree: &gix::Tree<'_>,
    prefix: &str,
    out: &mut Vec<String>,
) {
    for entry in tree.iter() {
        let Ok(entry) = entry else { continue };
        let name = entry.filename().to_str_lossy();
        let path = if prefix.is_empty() {
            name.into_owned()
        } else {
            format!("{prefix}/{name}")
        };
        if entry.mode().is_tree() {
            if let Ok(object) = repo.find_object(entry.oid().to_owned())
                && let Ok(subtree) = object.try_into_tree()
            {
                collect_paths(repo, &subtree, &path, out);
            }
        } else if entry.mode().is_blob() {
            out.push(path);
        }
    }
}

/// The ids of every ref directly under `prefix` (a meta-ref namespace,
/// e.g. `refs/meta/member/`) whose id contains `query` (case-insensitive),
/// capped via [`cap`] -- the same `state.refs.iter_prefix` listing
/// `crate::pages::members`/`crate::pages::effects` read their own rows
/// from, here matched against `query` instead of fully deserialized.
///
/// # Errors
///
/// Propagates a ref-store read failure.
fn meta_names<O>(state: &AppState<O>, prefix: &str, query: &str) -> Result<(Vec<String>, bool)> {
    if query.is_empty() {
        return Ok((Vec::new(), false));
    }
    let needle = query.to_lowercase();
    let mut names = Vec::new();
    for entry in state.refs.iter_prefix(prefix)? {
        let (name, _) = entry?;
        let path = name.as_bstr().to_string();
        if let Some(id) = path.strip_prefix(prefix)
            && id.to_lowercase().contains(&needle)
        {
            names.push(id.to_owned());
        }
    }
    Ok(cap(names))
}

/// Toolchain names containing `query` (case-insensitive), capped via
/// [`cap`] -- reads through the same [`toolchain::list`]
/// `crate::pages::toolchains::list` itself calls.
///
/// # Errors
///
/// Propagates a ref-store read failure.
fn search_toolchains<O>(state: &AppState<O>, query: &str) -> Result<(Vec<String>, bool)> {
    if query.is_empty() {
        return Ok((Vec::new(), false));
    }
    let needle = query.to_lowercase();
    let names = toolchain::list(state.refs.as_ref())?
        .into_iter()
        .filter(|name| name.to_lowercase().contains(&needle))
        .collect();
    Ok(cap(names))
}

/// One result group's card: `label` as its header, `rows` linked via
/// `href_for`, and a trailing "more matches not shown" row when `rows`
/// was capped. Renders nothing at all when `rows` is empty, so an
/// unmatched group leaves no empty card behind.
fn result_group(
    label: &str,
    rows: &[String],
    truncated: bool,
    href_for: impl Fn(&str) -> String,
) -> Markup {
    if rows.is_empty() {
        return html! {};
    }
    html! {
        div.card {
            div.card-header { (label) }
            ul.string-list {
                @for row in rows {
                    li { a href=(href_for(row)) { (row) } }
                }
            }
            @if truncated {
                div.card-row.muted { "More matches not shown." }
            }
        }
    }
}

/// The empty-results placeholder, shown for an empty query and for a
/// non-empty one with no matches at all.
fn blankslate(query: &str) -> Markup {
    html! {
        div.card {
            div.blankslate {
                h2 { "No matches" }
                @if query.is_empty() {
                    p { "Type a search term above to look through files and meta entities." }
                } @else {
                    p { "Nothing matched " code { (query) } "." }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "unit test")]

    use super::*;

    #[test]
    fn cap_truncates_and_reports_when_it_cut_something() {
        let (kept, truncated) = cap((0..150).map(|n| n.to_string()).collect());
        assert_eq!(kept.len(), MAX_RESULTS);
        assert!(truncated);

        let (kept, truncated) = cap(vec!["a".to_owned(), "b".to_owned()]);
        assert_eq!(kept.len(), 2);
        assert!(!truncated);
    }

    #[test]
    fn result_group_renders_nothing_for_an_empty_group() {
        let rendered = result_group("files", &[], false, |row| row.to_owned()).into_string();
        assert!(rendered.is_empty());
    }
}
