//! The repository's issues, sourced from the `refs/meta/issues/<id>` refs.
//!
//! Each issue is a self-contained typed document on its own ref,
//! `refs/meta/issues/<id>`, read and written through [`git_store`]. One ref per
//! issue keeps issues independently loadable and historied — the ref's commit
//! chain is the issue's edit history — and labels are plain strings so the index
//! can derive its filter set from whatever labels exist, with no separate label
//! registry to keep in sync.
//!
//! # Identity
//!
//! An issue carries two identifiers with two different jobs:
//!
//! * The **genesis key** — the ref's last segment, computed by [`new_id`] and
//!   never renamed — is the object id of the object the issue derives from (a
//!   review or proposal), or, when it derives from nothing, the hash of the
//!   issue's own initial content. Every issue is a git object, so there is no
//!   "no origin" case. Content-addressed and conflict-free: filing an issue
//!   never contends a counter, and one origin can never file the same issue
//!   twice. Cross-references (comments, reviews) key off this identifier, so
//!   it must never change.
//! * The **friendly number** — the `id` field, `None` until [`promote_with`]
//!   assigns one — is lifecycle state, not a key. `Option` is the one field
//!   kind `facet-git-tree` auto-defaults on an absent entry, so adding this
//!   field is backward compatible with every issue ref already on disk:
//!   nothing but promotion ever touches the shared counter that assigns it.

use std::path::Path;

use facet::Facet;

/// The namespace under which issues are recorded: one ref,
/// `refs/meta/issues/<id>`, per issue.
pub const ISSUES_NS: &str = "refs/meta/issues";

/// The ref holding the shared friendly-number counter. Only [`promote_with`]
/// advances it, so filing an issue never contends it.
pub const ISSUE_NUMBER_REF: &str = "refs/meta/issue-number";

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
    /// The friendly sequential number [`promote_with`] assigned, or `None`
    /// before a maintainer promotes the issue. Lifecycle state, not the
    /// issue's key — the ref's genesis hash is that.
    pub id: Option<String>,
}

impl Issue {
    /// Whether the issue is open (any state other than `closed`).
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.state != "closed"
    }
}

/// The `refs/meta/issue-number` document: the next friendly number a
/// promotion will assign.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Facet)]
struct IssueNumber {
    next: u64,
}

/// Derive an issue's stable genesis key: `origin`'s object id (hex) when the
/// issue derives from one — one origin, one issue, deduplicated on
/// provenance — otherwise the hash of the issue's own initial content, since
/// every issue is a git object and so always has one.
pub fn new_id(origin: Option<&str>, content: &Issue) -> Result<String, git_store::Error> {
    match origin {
        Some(origin) => Ok(origin.to_owned()),
        None => git_store::content_hash(content),
    }
}

/// Load the issue recorded at `refs/meta/issues/<id>` in `repo`, or `None` when
/// no such issue exists.
pub fn load(repo: &Path, id: &str) -> Result<Option<Issue>, git_store::Error> {
    git_store::Store::open(repo)?.load_item(ISSUES_NS, id)
}

/// Write `issue` to `refs/meta/issues/<id>` in `repo`, replacing any existing
/// value as a new commit so the ref's commit chain is the issue's edit history.
pub fn store(repo: &Path, id: &str, issue: &Issue) -> Result<(), git_store::Error> {
    git_store::Store::open(repo)?.store_item(ISSUES_NS, id, issue, "Update issue")
}

/// List every issue in `repo` as `(id, issue)` pairs, newest issue ref first.
pub fn list(repo: &Path) -> Result<Vec<(String, Issue)>, git_store::Error> {
    git_store::Store::open(repo)?.list_items(ISSUES_NS)
}

/// The number of open issues in `repo`.
pub fn open_count(repo: &Path) -> Result<usize, git_store::Error> {
    Ok(list(repo)?
        .into_iter()
        .filter(|(_id, issue)| issue.is_open())
        .count())
}

/// Why [`promote`] could not promote an issue.
#[derive(Debug, thiserror::Error)]
pub enum PromoteError {
    /// The underlying store failed to read or write a ref.
    #[error(transparent)]
    Store(#[from] git_store::Error),
    /// No issue is recorded at `id`.
    #[error("no issue at {0:?}")]
    NotFound(String),
}

/// How many times [`promote`] retries the counter CAS before giving up.
/// Bounds retry under sustained contention; ordinary races resolve in one or
/// two rounds.
const MAX_PROMOTE_RETRIES: usize = 5;

/// Promote the issue at the stable genesis key `id`: allocate the next
/// friendly number by CAS-incrementing [`ISSUE_NUMBER_REF`], then write it
/// into the issue's `id` field as a new commit on the *same* ref — the ref is
/// never renamed, so every cross-reference keyed off it still resolves.
///
/// The counter is advanced with [`Store::amend`](git_store::Store::amend),
/// not [`Store::store`](git_store::Store::store): two promotions racing for
/// the same number must never both succeed by merging, since a structural
/// merge would consider two identical successor values equal and let both
/// callers believe they claimed it. A CAS conflict here is retried by
/// re-reading the counter, so the number handed back is always the one
/// actually reserved for this call.
pub fn promote(repo: &Path, id: &str) -> Result<String, PromoteError> {
    let store = git_store::Store::open(repo)?;
    let mut number = None;
    for _ in 0..=MAX_PROMOTE_RETRIES {
        let current = store
            .load::<IssueNumber>(ISSUE_NUMBER_REF)?
            .unwrap_or(IssueNumber { next: 1 });
        let next = IssueNumber {
            next: current.next.saturating_add(1),
        };
        match store.amend(ISSUE_NUMBER_REF, &next, "Allocate issue number") {
            Ok(()) => {
                number = Some(current.next);
                break;
            }
            Err(git_store::Error::Conflict) => continue,
            Err(error) => return Err(error.into()),
        }
    }
    let number = number.ok_or(git_store::Error::Conflict)?.to_string();

    let mut issue = store
        .load_item::<Issue>(ISSUES_NS, id)?
        .ok_or_else(|| PromoteError::NotFound(id.to_owned()))?;
    issue.id = Some(number.clone());
    store.store_item(ISSUES_NS, id, &issue, "Update issue")?;
    Ok(number)
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
            id: None,
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

    #[test]
    fn new_id_uses_the_origin_when_one_is_given() {
        let content = issue("A bug", "open", &[]);
        assert_eq!(new_id(Some("deadbeef"), &content).unwrap(), "deadbeef");
    }

    #[test]
    fn new_id_hashes_its_own_content_with_no_origin() {
        let a = issue("A bug", "open", &[]);
        let b = issue("A different bug", "open", &[]);
        let a_id = new_id(None, &a).unwrap();
        let b_id = new_id(None, &b).unwrap();
        // Content-addressed: same content yields the same id, different
        // content yields a different one, with no counter involved.
        assert_eq!(a_id, new_id(None, &a).unwrap());
        assert_ne!(a_id, b_id);
    }

    #[test]
    fn filing_an_issue_leaves_its_friendly_number_unset() {
        let repo = unique_repo();
        let content = issue("A bug", "open", &[]);
        let id = new_id(None, &content).unwrap();
        store(&repo, &id, &content).unwrap();
        assert_eq!(load(&repo, &id).unwrap().unwrap().id, None);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn promotion_assigns_a_number_and_advances_the_counter_without_renaming_the_ref() {
        let repo = unique_repo();
        let content = issue("A bug", "open", &[]);
        let id = new_id(None, &content).unwrap();
        store(&repo, &id, &content).unwrap();

        let first = promote(&repo, &id).unwrap();
        assert_eq!(first, "1");
        let promoted = load(&repo, &id).unwrap().unwrap();
        assert_eq!(promoted.id, Some("1".to_owned()));

        // A second issue promotes to the next number; the first issue's ref
        // — keyed by its stable genesis hash — still resolves.
        let other = issue("Another bug", "open", &[]);
        let other_id = new_id(None, &other).unwrap();
        store(&repo, &other_id, &other).unwrap();
        assert_eq!(promote(&repo, &other_id).unwrap(), "2");
        assert_eq!(load(&repo, &id).unwrap().unwrap().id, Some("1".to_owned()));
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn promoting_an_absent_issue_fails() {
        let repo = unique_repo();
        assert!(matches!(
            promote(&repo, "missing"),
            Err(PromoteError::NotFound(id)) if id == "missing"
        ));
        let _ = std::fs::remove_dir_all(&repo);
    }
}
