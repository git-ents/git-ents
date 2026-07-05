//! Comments on code, one per ref under `refs/meta/comments/<id>`.
//!
//! Each comment is a self-contained typed document on its own ref, read and
//! written through [`git_store`] like an issue is, and anchored to a blob (and
//! optionally a line range) through [`git_anchor`]: the stored [`Anchor`] is
//! authoritative at creation and never mutated, and [`project`] re-derives at
//! read time where the comment sits on any other commit.
//!
//! # The comment is the commit
//!
//! The document tree holds only the body, the anchor, and an optional issue
//! cross-reference. Who wrote the comment and when are *not* fields: they are
//! recovered from the ref's commit chain ([`provenance`]) — the genesis
//! commit's author created the comment, the tip commit's author last edited
//! it — exactly as git itself carries authorship. [`store`] therefore takes
//! the author and stamps it on the commit it writes.
//!
//! # Identity
//!
//! A comment's key is its ref's genesis hash, computed by [`new_id`] and never
//! renamed: the object id of the object the comment derives from, or the hash
//! of its own initial content when it derives from nothing. Cross-references
//! key off this identifier, matching the issues collection's scheme.

use std::path::Path;

use facet::Facet;
use git_anchor::{Anchor, Projection};
use git_store::Provenance;

// @relation(comments.ref)
/// The namespace under which comments are recorded: one ref,
/// `refs/meta/comments/<id>`, per comment.
pub const COMMENTS_NS: &str = "refs/meta/comments";

/// One comment stored at `refs/meta/comments/<id>`. Author and timestamp are
/// deliberately absent: they live on the ref's commits (see [`provenance`]).
///
/// ## Requirements
///
/// @relation(comments.ref, comments.authorship, comments.anchor, comments.projection)
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct Comment {
    /// The comment's body text.
    pub body: String,
    /// Where the comment was written — the commit, path, blob, and optional
    /// line range it was anchored to at creation.
    pub anchor: Anchor,
    /// The genesis id of the issue the comment belongs to, or `None` for a
    /// free-standing comment.
    pub issue: Option<String>,
}

/// Derive a comment's stable genesis key: `origin`'s object id (hex) when the
/// comment derives from one, otherwise the hash of the comment's own initial
/// content — every comment is a git object, so it always has one.
pub fn new_id(origin: Option<&str>, content: &Comment) -> Result<String, git_store::Error> {
    git_store::new_id(origin, content)
}

/// Load the comment recorded at `refs/meta/comments/<id>` in `repo`, or `None`
/// when no such comment exists.
pub fn load(repo: &Path, id: &str) -> Result<Option<Comment>, git_store::Error> {
    git_store::Store::open(repo)?.load_item(COMMENTS_NS, id)
}

/// Write `comment` to `refs/meta/comments/<id>` in `repo` as a new commit
/// authored by `author` (a `(name, email)` pair), so the ref's commit chain is
/// the comment's edit history and carries its authorship.
///
/// ## Requirements
///
/// @relation(comments.ref, comments.authorship)
pub fn store(
    repo: &Path,
    id: &str,
    comment: &Comment,
    author: (&str, &str),
) -> Result<(), git_store::Error> {
    git_store::Store::open(repo)?.store_item_authored(
        COMMENTS_NS,
        id,
        comment,
        "Update comment",
        author,
    )
}

/// List every comment in `repo` as `(id, comment)` pairs, newest ref first.
pub fn list(repo: &Path) -> Result<Vec<(String, Comment)>, git_store::Error> {
    git_store::Store::open(repo)?.list_items(COMMENTS_NS)
}

/// Who created and who last updated the comment at `id`, recovered from its
/// ref's commit chain, or `None` when no such comment exists.
///
/// ## Requirements
///
/// @relation(comments.authorship)
pub fn provenance(repo: &Path, id: &str) -> Result<Option<Provenance>, git_store::Error> {
    git_store::Store::open(repo)?.item_provenance(COMMENTS_NS, id)
}

/// Where `comment`'s anchor sits on `target` (a revision in `repo`): still
/// [`Projection::Current`], relocated to a new path or shifted lines, outdated
/// because the anchored region was edited, or gone with its file.
///
/// ## Requirements
///
/// @relation(comments.projection)
pub fn project(
    repo: &Path,
    comment: &Comment,
    target: &str,
) -> Result<Projection, git_anchor::Error> {
    git_anchor::project(repo, &comment.anchor, target)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::let_underscore_must_use,
        reason = "unit test"
    )]

    use std::process::Command;

    use git_anchor::LineRange;
    use git_store::test_support::{commit_all, git_with_stdin, repo};

    use super::*;

    fn comment(body: &str, issue: Option<&str>) -> Comment {
        Comment {
            body: body.to_owned(),
            anchor: Anchor {
                commit: "0123456789abcdef0123456789abcdef01234567".into(),
                path: "src/lib.rs".to_owned(),
                blob: "89abcdef0123456789abcdef0123456789abcdef".into(),
                lines: Some(LineRange { start: 3, end: 4 }),
            },
            issue: issue.map(str::to_owned),
        }
    }

    const AUTHOR: (&str, &str) = ("alice", "alice@example.com");

    // @relation(comments.ref, role=Verifies)
    #[test]
    fn store_then_load_round_trips_a_comment() {
        let dir = repo();
        let written = comment("Why is this 1?", Some("deadbeef"));
        store(dir.path(), "1", &written, AUTHOR).unwrap();
        assert_eq!(load(dir.path(), "1").unwrap(), Some(written));
    }

    #[test]
    fn none_when_the_comment_is_absent() {
        let dir = repo();
        assert_eq!(load(dir.path(), "1").unwrap(), None);
        assert_eq!(provenance(dir.path(), "1").unwrap(), None);
    }

    #[test]
    fn lists_comments_keyed_by_id() {
        let dir = repo();
        store(dir.path(), "1", &comment("first", None), AUTHOR).unwrap();
        store(dir.path(), "2", &comment("second", None), AUTHOR).unwrap();
        let mut ids: Vec<String> = list(dir.path())
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        ids.sort();
        assert_eq!(ids, vec!["1".to_owned(), "2".to_owned()]);
    }

    #[test]
    fn new_id_uses_the_origin_when_one_is_given() {
        let content = comment("a comment", None);
        assert_eq!(new_id(Some("deadbeef"), &content).unwrap(), "deadbeef");
    }

    #[test]
    fn new_id_hashes_its_own_content_with_no_origin() {
        let a = comment("a comment", None);
        let b = comment("a different comment", None);
        let a_id = new_id(None, &a).unwrap();
        assert_eq!(a_id, new_id(None, &a).unwrap());
        assert_ne!(a_id, new_id(None, &b).unwrap());
    }

    // @relation(comments.authorship, role=Verifies)
    #[test]
    fn provenance_comes_from_the_commits_not_the_document() {
        let dir = repo();
        store(dir.path(), "1", &comment("first", None), AUTHOR).unwrap();
        store(
            dir.path(),
            "1",
            &comment("edited", None),
            ("bob", "bob@example.com"),
        )
        .unwrap();
        let provenance = provenance(dir.path(), "1").unwrap().unwrap();
        assert_eq!(provenance.created.name, "alice");
        assert_eq!(provenance.created.email, "alice@example.com");
        assert_eq!(provenance.updated.name, "bob");
        assert!(provenance.created.seconds > 0);
    }

    // @relation(comments.anchor, comments.projection, role=Verifies)
    #[test]
    fn a_stored_comment_projects_onto_the_commit_it_was_written_against() {
        let dir = repo();
        std::fs::write(dir.path().join("file.txt"), "one\ntwo\nthree\n").unwrap();
        commit_all(dir.path(), "one");

        let anchor = git_anchor::capture(
            dir.path(),
            "HEAD",
            "file.txt",
            Some(LineRange { start: 2, end: 2 }),
        )
        .unwrap();
        let written = Comment {
            body: "Why two?".to_owned(),
            anchor,
            issue: None,
        };
        let id = new_id(None, &written).unwrap();
        store(dir.path(), &id, &written, AUTHOR).unwrap();

        let loaded = load(dir.path(), &id).unwrap().unwrap();
        assert_eq!(
            git_anchor::snippet(dir.path(), &loaded.anchor).unwrap(),
            "two\n"
        );
        assert_eq!(
            project(dir.path(), &loaded, "HEAD").unwrap(),
            Projection::Current
        );
    }

    // @relation(comments.anchor, storage.meta-ref, role=Verifies)
    #[test]
    fn loads_the_on_disk_comment_format() {
        // A fixture written as the real on-disk layout — a `body` blob, an
        // `anchor/` subtree of `commit`/`path`/`blob` blobs with a
        // `lines/some/{start,end}` Option subtree, and an `issue/some` Option
        // blob — must keep loading, guarding the Comment document's shape
        // against an incompatible change to data already on a ref.
        let dir = repo();
        let repo = dir.path();
        let blob = |value: &str| git_with_stdin(repo, &["hash-object", "-w", "--stdin"], value);

        let expected = comment("Why is this 1?", Some("deadbeef"));
        let range = expected.anchor.lines.unwrap();
        let range_tree = git_with_stdin(
            repo,
            &["mktree"],
            &format!(
                "100644 blob {}\tstart\n100644 blob {}\tend\n",
                blob(&range.start.to_string()),
                blob(&range.end.to_string()),
            ),
        );
        let lines_tree = git_with_stdin(
            repo,
            &["mktree"],
            &format!("040000 tree {range_tree}\tsome\n"),
        );
        let anchor_tree = git_with_stdin(
            repo,
            &["mktree"],
            &format!(
                "100644 blob {}\tcommit\n\
                 100644 blob {}\tpath\n\
                 100644 blob {}\tblob\n\
                 040000 tree {lines_tree}\tlines\n",
                blob(&expected.anchor.commit.to_string()),
                blob(&expected.anchor.path),
                blob(&expected.anchor.blob.to_string()),
            ),
        );
        let issue_tree = git_with_stdin(
            repo,
            &["mktree"],
            &format!(
                "100644 blob {}\tsome\n",
                blob(expected.issue.as_deref().unwrap())
            ),
        );
        let root = git_with_stdin(
            repo,
            &["mktree"],
            &format!(
                "100644 blob {}\tbody\n\
                 040000 tree {anchor_tree}\tanchor\n\
                 040000 tree {issue_tree}\tissue\n",
                blob(&expected.body),
            ),
        );
        let commit = git_with_stdin(repo, &["commit-tree", &root, "-m", "fixture"], "");
        let status = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["update-ref", &format!("{COMMENTS_NS}/1"), &commit])
            .status()
            .unwrap();
        assert!(status.success());

        assert_eq!(load(repo, "1").unwrap(), Some(expected));
    }
}
