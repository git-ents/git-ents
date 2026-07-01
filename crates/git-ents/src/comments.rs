//! Comments on issues, sourced from the
//! `refs/meta/comments/<issue_id>/<comment_id>` refs.
//!
//! Each comment is its own document nested under the issue's stable genesis
//! key — never its friendly number, so a comment filed before promotion still
//! resolves after. The comment's own id is the ref's last segment: the hash of
//! its own content, content-addressed exactly like an issue's genesis key
//! ([`git_ents::issues`](crate::issues)), so concurrent additions never
//! collide and no counter is needed.

use std::path::Path;

use facet::Facet;

/// The namespace under which comments are recorded: one ref,
/// `refs/meta/comments/<issue_id>/<comment_id>`, per comment.
pub const COMMENTS_NS: &str = "refs/meta/comments";

/// One comment on an issue, stored at
/// `refs/meta/comments/<issue_id>/<comment_id>`.
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct Comment {
    /// The comment's body text.
    pub body: String,
    /// The identity that wrote the comment.
    pub author: String,
    /// The commented-on issue's stable genesis key — never its friendly
    /// number, so a comment filed before promotion still resolves after.
    pub issue_id: String,
}

/// The ref namespace holding every comment on `issue_id`.
fn issue_comments_ns(issue_id: &str) -> String {
    format!("{COMMENTS_NS}/{issue_id}")
}

/// Derive a comment's stable id: the hash of its own content, since a comment
/// derives from nothing upstream and so always originates itself — the same
/// content-addressing [`issues::new_id`](crate::issues::new_id) uses when an
/// issue has no origin either.
pub fn new_id(content: &Comment) -> Result<String, git_store::Error> {
    git_store::content_hash(content)
}

/// Load the comment `comment_id` on `issue_id` in `repo`.
pub fn load(
    repo: &Path,
    issue_id: &str,
    comment_id: &str,
) -> Result<Option<Comment>, git_store::Error> {
    git_store::Store::open(repo)?.load_item(&issue_comments_ns(issue_id), comment_id)
}

/// Write `comment` at `refs/meta/comments/<issue_id>/<comment_id>` in `repo`,
/// where `comment_id` is [`new_id`] of `comment`.
pub fn store(
    repo: &Path,
    issue_id: &str,
    comment_id: &str,
    comment: &Comment,
) -> Result<(), git_store::Error> {
    git_store::Store::open(repo)?.store_item(
        &issue_comments_ns(issue_id),
        comment_id,
        comment,
        "Add comment",
    )
}

/// List every comment on `issue_id` in `repo`, as `(comment_id, comment)`
/// pairs, newest first.
pub fn list(repo: &Path, issue_id: &str) -> Result<Vec<(String, Comment)>, git_store::Error> {
    git_store::Store::open(repo)?.list_items(&issue_comments_ns(issue_id))
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::let_underscore_must_use,
        reason = "unit test"
    )]

    use super::*;
    use crate::testutil::unique_repo as new_repo;

    fn unique_repo() -> std::path::PathBuf {
        new_repo("comments")
    }

    fn comment(issue_id: &str, body: &str) -> Comment {
        Comment {
            body: body.to_owned(),
            author: "alice".to_owned(),
            issue_id: issue_id.to_owned(),
        }
    }

    #[test]
    fn store_then_load_round_trips_a_comment() {
        let repo = unique_repo();
        let written = comment("issue-1", "A comment");
        let id = new_id(&written).unwrap();
        store(&repo, "issue-1", &id, &written).unwrap();
        assert_eq!(load(&repo, "issue-1", &id).unwrap(), Some(written));
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn none_when_the_comment_is_absent() {
        let repo = unique_repo();
        assert_eq!(load(&repo, "issue-1", "missing").unwrap(), None);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn lists_only_the_comments_on_the_named_issue() {
        let repo = unique_repo();
        let a = comment("issue-1", "First");
        let b = comment("issue-1", "Second");
        let other = comment("issue-2", "Unrelated");
        store(&repo, "issue-1", &new_id(&a).unwrap(), &a).unwrap();
        store(&repo, "issue-1", &new_id(&b).unwrap(), &b).unwrap();
        store(&repo, "issue-2", &new_id(&other).unwrap(), &other).unwrap();

        let mut bodies: Vec<String> = list(&repo, "issue-1")
            .unwrap()
            .into_iter()
            .map(|(_id, comment)| comment.body)
            .collect();
        bodies.sort();
        assert_eq!(bodies, vec!["First".to_owned(), "Second".to_owned()]);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn new_id_is_content_addressed() {
        let a = comment("issue-1", "Same text");
        let b = comment("issue-1", "Same text");
        let different = comment("issue-1", "Different text");
        assert_eq!(new_id(&a).unwrap(), new_id(&b).unwrap());
        assert_ne!(new_id(&a).unwrap(), new_id(&different).unwrap());
    }

    #[test]
    fn a_comment_filed_before_promotion_still_resolves_after() {
        let repo = unique_repo();
        let issue = crate::issues::Issue {
            title: "A bug".to_owned(),
            body: "A body".to_owned(),
            state: "open".to_owned(),
            labels: vec![],
            author: "alice".to_owned(),
            id: None,
        };
        let issue_id = crate::issues::new_id(None, &issue).unwrap();
        crate::issues::store(&repo, &issue_id, &issue).unwrap();

        let written = comment(&issue_id, "Filed before promotion");
        let comment_id = new_id(&written).unwrap();
        store(&repo, &issue_id, &comment_id, &written).unwrap();

        crate::issues::promote(&repo, &issue_id).unwrap();

        // The comment is keyed off the stable genesis id, which promotion
        // never renames, so it still resolves.
        assert_eq!(load(&repo, &issue_id, &comment_id).unwrap(), Some(written));
        let _ = std::fs::remove_dir_all(&repo);
    }
}
