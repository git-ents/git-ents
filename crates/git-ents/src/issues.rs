//! The repository's issues, sourced from the `refs/meta/issues/<id>` refs.
//!
//! Each issue is a self-contained typed document on its own ref,
//! `refs/meta/issues/<id>`, read and written through [`git_store`]. One ref per
//! issue keeps issues independently loadable and historied — the ref's commit
//! chain is the issue's edit history — and labels are plain strings so the index
//! can derive its filter set from whatever labels exist, with no separate label
//! registry to keep in sync.

use std::path::Path;

use facet::Facet;

/// The namespace under which issues are recorded: one ref,
/// `refs/meta/issues/<id>`, per issue.
pub const ISSUES_NS: &str = "refs/meta/issues";

/// One issue stored at `refs/meta/issues/<id>`.
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct Issue {
    /// The issue's one-line title.
    pub title: String,
    /// The issue's body text.
    pub body: String,
    /// The issue's state — `open` or `closed`.
    pub state: String,
    /// The labels applied to the issue, as plain strings.
    pub labels: Vec<String>,
    /// The identity that opened the issue.
    pub author: String,
}

impl Issue {
    /// Whether the issue is open (any state other than `closed`).
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.state != "closed"
    }
}

/// Load the issue recorded at `refs/meta/issues/<id>` in `repo`, or `None` when
/// no such issue exists.
pub fn load(repo: &Path, id: &str) -> Result<Option<Issue>, git_store::Error> {
    git_store::Store::open(repo)?.load::<Issue>(&format!("{ISSUES_NS}/{id}"))
}

/// Write `issue` to `refs/meta/issues/<id>`, replacing any existing value, as a
/// new commit so the ref's commit chain is the issue's edit history.
pub fn store(repo: &Path, id: &str, issue: &Issue) -> Result<(), git_store::Error> {
    git_store::Store::open(repo)?.store(&format!("{ISSUES_NS}/{id}"), issue, "Update issue")?;
    Ok(())
}

/// List every issue as `(id, issue)` pairs, newest issue ref first.
pub fn list(repo: &Path) -> Result<Vec<(String, Issue)>, git_store::Error> {
    let store = git_store::Store::open(repo)?;
    let prefix = format!("{ISSUES_NS}/");
    let mut issues = Vec::new();
    for refname in store.list(&prefix)? {
        let Some(id) = refname.strip_prefix(&prefix) else {
            continue;
        };
        if let Some(issue) = store.load::<Issue>(&refname)? {
            issues.push((id.to_owned(), issue));
        }
    }
    Ok(issues)
}

/// The number of open issues in `repo`.
pub fn open_count(repo: &Path) -> Result<usize, git_store::Error> {
    Ok(list(repo)?
        .into_iter()
        .filter(|(_id, issue)| issue.is_open())
        .count())
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::let_underscore_must_use,
        reason = "unit test"
    )]

    use super::*;
    use crate::testutil::{unique_repo as new_repo, write_issue_doc};

    fn unique_repo() -> std::path::PathBuf {
        new_repo("issues")
    }

    fn issue(title: &str, state: &str, labels: &[&str]) -> Issue {
        Issue {
            title: title.to_owned(),
            body: "A body".to_owned(),
            state: state.to_owned(),
            labels: labels.iter().map(|l| (*l).to_owned()).collect(),
            author: "alice".to_owned(),
        }
    }

    #[test]
    fn store_then_load_round_trips_an_issue() {
        let repo = unique_repo();
        let written = issue("A bug", "open", &["bug", "p1"]);
        store(&repo, "1", &written).unwrap();
        assert_eq!(load(&repo, "1").unwrap(), Some(written));
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn none_when_the_issue_is_absent() {
        let repo = unique_repo();
        assert_eq!(load(&repo, "1").unwrap(), None);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn lists_issues_and_counts_the_open_ones() {
        let repo = unique_repo();
        store(&repo, "1", &issue("Open one", "open", &["bug"])).unwrap();
        store(&repo, "2", &issue("Closed one", "closed", &[])).unwrap();
        let mut ids: Vec<String> = list(&repo).unwrap().into_iter().map(|(id, _)| id).collect();
        ids.sort();
        assert_eq!(ids, vec!["1".to_owned(), "2".to_owned()]);
        assert_eq!(open_count(&repo).unwrap(), 1);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn loads_the_on_disk_issue_format() {
        // A fixture written as the real on-disk layout — `title`, `body`,
        // `state`, `author` blobs plus an index-keyed `labels/` subtree — must
        // keep loading, guarding the Issue document's shape against an
        // incompatible change to data already on a ref.
        let repo = unique_repo();
        write_issue_doc(
            &repo,
            &format!("{ISSUES_NS}/1"),
            "A bug",
            "A body",
            "open",
            &["bug", "p1"],
            "alice",
        );
        assert_eq!(
            load(&repo, "1").unwrap(),
            Some(issue("A bug", "open", &["bug", "p1"]))
        );
        let _ = std::fs::remove_dir_all(&repo);
    }
}
