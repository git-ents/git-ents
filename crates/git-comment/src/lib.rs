//! Comments on code, one per ref under `refs/meta/comments/<id>`.
//!
//! Each comment is a self-contained typed document on its own ref, read and
//! written through [`git_store`] like an issue is, and anchored to a blob (and
//! optionally a line range) through [`git_anchor`]: the stored [`Anchor`] is
//! authoritative at creation and never mutated, and [`project`] re-derives at
//! read time where the comment sits on any other commit.
//!
//! # Retention
//!
//! Nothing pins the anchored commit against garbage collection any more — its
//! id on [`Anchor::commit`] is best-effort. What actually survives is the
//! anchored *content*: [`store`] embeds the anchored blob directly (a tree
//! entry pointing at its existing object id, no copy — content addressing
//! makes this free) alongside a small `context` blob of the surrounding source
//! lines ([`git_anchor::context`]), both as ordinary entries in the comment's
//! own document tree. That makes them reachable — and so un-collectable — for
//! as long as the comment's ref exists, with no gitlink and no second commit
//! parent involved. [`project`] uses the context blob to fuzzy-match the
//! anchor's location back onto a target commit once the anchor commit itself
//! is gone (see [`git_anchor::project_from_context`]).
//!
//! # The comment is the commit
//!
//! The document tree holds only the body, the anchor, and an optional issue
//! cross-reference (plus the retained blob and context, invisible to the
//! public [`Comment`] type — see [`StoredComment`]). Who wrote the comment and
//! when are *not* fields: they are recovered from the ref's commit chain
//! ([`provenance`]) — the genesis commit's author created the comment, the
//! tip commit's author last edited it — exactly as git itself carries
//! authorship. [`store`] therefore takes the author and stamps it on the
//! commit it writes.
//!
//! # Identity
//!
//! A comment's key is its ref's genesis hash, computed by [`new_id`] and never
//! renamed: the object id of the object the comment derives from, or the hash
//! of its own initial content when it derives from nothing. Cross-references
//! key off this identifier, matching the issues collection's scheme.

use std::path::Path;

use facet::Facet;
use facet_git_tree::RawTree;
use git_anchor::{Anchor, Projection};
use git_store::Provenance;
use gix::ObjectId;
use gix::objs::tree::{Entry as TreeEntry, EntryKind, EntryMode};
use gix::objs::{Blob, FindExt as _, Tree, Write as _};

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

/// The document actually written to and read from a comment's ref: [`Comment`]
/// plus `retained`, a passthrough tree ([`facet_git_tree::RawTree`]) holding
/// two entries invisible to [`Comment`] itself — `blob`, the anchored file at
/// its own object id (a reference, not a copy: content addressing makes this
/// free), and `context`, [`git_anchor::context`]'s snapshot of the
/// surrounding lines. Both ride along in the comment's own document tree
/// purely so they stay reachable from `refs/meta/comments/<id>` — and so
/// survive force-push, branch deletion, and gc — for as long as the comment's
/// ref exists, with no gitlink and no second commit parent involved.
///
/// `retained` is deliberately absent from the public [`Comment`]: it is
/// storage plumbing a caller never needs to see, read, or set — [`store`]
/// derives it fresh from `comment.anchor` every write.
///
/// ## Requirements
///
/// @relation(anchor.reachability)
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
struct StoredComment {
    body: String,
    anchor: Anchor,
    issue: Option<String>,
    retained: RawTree,
}

impl From<StoredComment> for Comment {
    fn from(stored: StoredComment) -> Self {
        Self {
            body: stored.body,
            anchor: stored.anchor,
            issue: stored.issue,
        }
    }
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
    Ok(git_store::Store::open(repo)?
        .load_item::<StoredComment>(COMMENTS_NS, id)?
        .map(Into::into))
}

/// Write `comment` to `refs/meta/comments/<id>` in `repo` as a new commit
/// authored by `author` (a `(name, email)` pair), so the ref's commit chain is
/// the comment's edit history and carries its authorship. Also embeds the
/// anchored blob and a context snapshot in the written document tree (see
/// [`StoredComment`]), so the content the comment is anchored to stays
/// reachable independently of whether `comment.anchor.commit` itself survives.
///
/// ## Requirements
///
/// @relation(comments.ref, comments.authorship, anchor.reachability)
pub fn store(
    repo: &Path,
    id: &str,
    comment: &Comment,
    author: (&str, &str),
) -> Result<(), git_store::Error> {
    let context = git_anchor::context(repo, &comment.anchor)
        .map_err(|error| git_store::Error::Invalid(error.to_string()))?;
    let odb = odb_at(repo)?;
    let retained = embed(&odb, &comment.anchor, &context)?;
    let stored = StoredComment {
        body: comment.body.clone(),
        anchor: comment.anchor.clone(),
        issue: comment.issue.clone(),
        retained,
    };
    git_store::Store::open(repo)?.store_item_authored(
        COMMENTS_NS,
        id,
        &stored,
        "Update comment",
        author,
    )
}

/// Write the anchored blob (by its existing object id, no copy) and a fresh
/// `context` blob into a small tree, wrapped as a [`RawTree`] ready to embed
/// in a [`StoredComment`] — the retention mechanism [`store`] relies on.
///
/// ## Requirements
///
/// @relation(anchor.reachability)
fn embed(
    odb: &gix::odb::Handle,
    anchor: &Anchor,
    context: &str,
) -> Result<RawTree, git_store::Error> {
    let blob_oid = ObjectId::try_from(&anchor.blob)
        .map_err(|error| git_store::Error::Invalid(error.to_string()))?;
    let context_oid = odb
        .write(&Blob {
            data: context.as_bytes().to_vec(),
        })
        .map_err(|error| git_store::Error::Object(error.to_string()))?;
    let mut entries = vec![
        TreeEntry {
            mode: EntryMode::from(EntryKind::Blob),
            filename: "blob".into(),
            oid: blob_oid,
        },
        TreeEntry {
            mode: EntryMode::from(EntryKind::Blob),
            filename: "context".into(),
            oid: context_oid,
        },
    ];
    entries.sort();
    let tree_oid = odb
        .write(&Tree { entries })
        .map_err(|error| git_store::Error::Object(error.to_string()))?;
    Ok(RawTree::new(tree_oid))
}

/// Open a raw object database on `repo`'s common git directory, the same one
/// [`git_store::Store`] uses internally — opened again here since writing the
/// retained blob and context tree directly is this crate's own concern (see
/// [`embed`]), the same reasoning `git-toolchain` documents for its own
/// direct object writes.
fn odb_at(repo: &Path) -> Result<gix::odb::Handle, git_store::Error> {
    let opened = gix::open(repo).map_err(|error| git_store::Error::Open(Box::new(error)))?;
    gix::odb::at(opened.common_dir().join("objects")).map_err(|_io| git_store::Error::Odb)
}

/// List every comment in `repo` as `(id, comment)` pairs, newest ref first.
pub fn list(repo: &Path) -> Result<Vec<(String, Comment)>, git_store::Error> {
    Ok(git_store::Store::open(repo)?
        .list_items::<StoredComment>(COMMENTS_NS)?
        .into_iter()
        .map(|(id, stored)| (id, stored.into()))
        .collect())
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

/// Where the comment `id`'s anchor sits on `target` (a revision in `repo`):
/// still [`Projection::Current`], relocated to a new path or shifted lines,
/// outdated because the anchored region was edited, or gone with its file.
///
/// Tries [`git_anchor::project`] first; if the comment's anchored commit has
/// been garbage collected, falls back to [`git_anchor::project_from_context`]
/// against the comment's retained `context` blob — recomputing the context
/// would need the very commit that is gone.
///
/// ## Requirements
///
/// @relation(comments.projection)
pub fn project(repo: &Path, id: &str, target: &str) -> Result<Projection, git_anchor::Error> {
    let stored: StoredComment = git_store::Store::open(repo)
        .and_then(|store| store.load_item(COMMENTS_NS, id))
        .map_err(|error| git_anchor::Error::Object(error.to_string()))?
        .ok_or_else(|| git_anchor::Error::Object(format!("{COMMENTS_NS}/{id} does not exist")))?;
    match git_anchor::project(repo, &stored.anchor, target) {
        Err(git_anchor::Error::AnchorCommitMissing(_)) => {
            let context = retained_context(repo, &stored)
                .map_err(|error| git_anchor::Error::Object(error.to_string()))?;
            git_anchor::project_from_context(repo, &stored.anchor, target, &context)
        }
        other => other,
    }
}

/// Read the `context` blob out of `stored`'s retained tree (see
/// [`StoredComment`]), for [`project`]'s fallback path.
fn retained_context(repo: &Path, stored: &StoredComment) -> Result<String, git_store::Error> {
    let odb = odb_at(repo)?;
    let mut tree_buf = Vec::new();
    let tree = odb
        .find_tree(&stored.retained.oid(), &mut tree_buf)
        .map_err(|error| git_store::Error::Object(error.to_string()))?;
    let entry = tree
        .entries
        .iter()
        .find(|entry| entry.filename == "context")
        .ok_or_else(|| git_store::Error::Object("retained tree has no context entry".to_owned()))?;
    let mut blob_buf = Vec::new();
    let blob = odb
        .find_blob(entry.oid, &mut blob_buf)
        .map_err(|error| git_store::Error::Object(error.to_string()))?;
    Ok(String::from_utf8_lossy(blob.data).into_owned())
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::let_underscore_must_use,
        reason = "unit test"
    )]

    use std::path::Path;
    use std::process::Command;

    use git_anchor::LineRange;
    use git_store::test_support::{commit_all, git_with_stdin, repo};

    use super::*;

    /// A comment whose anchor points at a real (if otherwise arbitrary) blob
    /// in `dir` — `store` now has to read that blob to derive `context`, so a
    /// fixture anchored to a made-up oid would fail before ever reaching the
    /// assertions these tests care about. The anchor's `commit` stays a
    /// made-up hex string: nothing reads it back except as an opaque field.
    fn comment(dir: &Path, body: &str, issue: Option<&str>) -> Comment {
        let blob = git_with_stdin(
            dir,
            &["hash-object", "-w", "--stdin"],
            "one\ntwo\nthree\nfour\n",
        );
        Comment {
            body: body.to_owned(),
            anchor: Anchor {
                commit: "0123456789abcdef0123456789abcdef01234567".into(),
                path: "src/lib.rs".to_owned(),
                blob: blob.as_str().into(),
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
        let written = comment(dir.path(), "Why is this 1?", Some("deadbeef"));
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
        store(dir.path(), "1", &comment(dir.path(), "first", None), AUTHOR).unwrap();
        store(
            dir.path(),
            "2",
            &comment(dir.path(), "second", None),
            AUTHOR,
        )
        .unwrap();
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
        let dir = repo();
        let content = comment(dir.path(), "a comment", None);
        assert_eq!(new_id(Some("deadbeef"), &content).unwrap(), "deadbeef");
    }

    #[test]
    fn new_id_hashes_its_own_content_with_no_origin() {
        let dir = repo();
        let a = comment(dir.path(), "a comment", None);
        let b = comment(dir.path(), "a different comment", None);
        let a_id = new_id(None, &a).unwrap();
        assert_eq!(a_id, new_id(None, &a).unwrap());
        assert_ne!(a_id, new_id(None, &b).unwrap());
    }

    // @relation(comments.authorship, role=Verifies)
    #[test]
    fn provenance_comes_from_the_commits_not_the_document() {
        let dir = repo();
        store(dir.path(), "1", &comment(dir.path(), "first", None), AUTHOR).unwrap();
        store(
            dir.path(),
            "1",
            &comment(dir.path(), "edited", None),
            ("bob", "bob@example.com"),
        )
        .unwrap();
        let provenance = provenance(dir.path(), "1").unwrap().unwrap();
        assert_eq!(provenance.created.name, "alice");
        assert_eq!(provenance.created.email, "alice@example.com");
        assert_eq!(provenance.updated.name, "bob");
        assert!(provenance.created.seconds > 0);
    }

    /// The current branch's short name, so a test that force-moves the
    /// branch ref does not have to guess `init.defaultBranch`.
    fn current_branch(dir: &Path) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["symbolic-ref", "--short", "HEAD"])
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    /// Whether `oid` still exists as an object in `dir`'s repository.
    fn object_exists(dir: &Path, oid: &str) -> bool {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["cat-file", "-e", oid])
            .status()
            .unwrap()
            .success()
    }

    // @relation(anchor.reachability, role=Verifies)
    #[test]
    fn the_anchored_blob_survives_branch_deletion_and_gc_pruning_the_anchor_commit() {
        let dir = repo();
        let repo_path = dir.path();
        std::fs::write(repo_path.join("file.txt"), "one\ntwo\nthree\nfour\nfive\n").unwrap();
        commit_all(repo_path, "one");

        let anchor = git_anchor::capture(
            repo_path,
            "HEAD",
            "file.txt",
            Some(LineRange { start: 2, end: 2 }),
        )
        .unwrap();
        let anchor_commit = anchor.commit.to_string();
        let written = Comment {
            body: "why two?".to_owned(),
            anchor,
            issue: None,
        };
        let id = new_id(None, &written).unwrap();
        store(repo_path, &id, &written, AUTHOR).unwrap();

        // Rewrite the branch onto a brand-new parentless commit holding an
        // edited file, so the original commit is no longer anyone's
        // ancestor. An ordinary edit could never detach history like this,
        // but a rebase, a `filter-repo` pass, or a force-push can, and that
        // is exactly the scenario retention has to survive.
        let edited_blob = git_with_stdin(
            repo_path,
            &["hash-object", "-w", "--stdin"],
            "zero\none\ntwo\nthree\nfour\nfive\n",
        );
        let edited_tree = git_with_stdin(
            repo_path,
            &["mktree"],
            &format!("100644 blob {edited_blob}\tfile.txt\n"),
        );
        let replacement =
            git_with_stdin(repo_path, &["commit-tree", &edited_tree, "-m", "two"], "");
        let branch = current_branch(repo_path);
        let status = Command::new("git")
            .arg("-C")
            .arg(repo_path)
            .args(["update-ref", &format!("refs/heads/{branch}"), &replacement])
            .status()
            .unwrap();
        assert!(status.success());
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(repo_path)
                .args(["reflog", "expire", "--expire=now", "--all"])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(repo_path)
                .args(["gc", "--prune=now", "--quiet"])
                .status()
                .unwrap()
                .success()
        );

        assert!(
            !object_exists(repo_path, &anchor_commit),
            "the anchor commit should have been pruned"
        );

        // The anchored blob, embedded in the comment's own tree, is still
        // readable straight off the ref...
        let loaded = load(repo_path, &id).unwrap().unwrap();
        assert_eq!(
            git_anchor::snippet(repo_path, &loaded.anchor).unwrap(),
            "two\n"
        );
        // ...and still projects onto the rewritten branch tip, via the
        // context fallback `project` reaches for once the anchor commit is
        // gone.
        assert_eq!(
            project(repo_path, &id, &replacement).unwrap(),
            Projection::Relocated {
                path: "file.txt".to_owned(),
                lines: Some(LineRange { start: 3, end: 3 }),
            }
        );
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
            project(dir.path(), &id, "HEAD").unwrap(),
            Projection::Current
        );
    }

    // @relation(comments.anchor, storage.meta-ref, role=Verifies)
    #[test]
    fn loads_the_on_disk_comment_format() {
        // A fixture written as the real on-disk layout — a `body` blob, an
        // `anchor/` subtree of `commit`/`path`/`blob` blobs with a
        // `lines/some/{start,end}` Option subtree, an `issue/some` Option
        // blob, and a `retained/{blob,context}` passthrough tree — must keep
        // loading, guarding the document's shape against an incompatible
        // change to data already on a ref.
        let dir = repo();
        let repo = dir.path();
        let blob = |value: &str| git_with_stdin(repo, &["hash-object", "-w", "--stdin"], value);

        let expected = comment(repo, "Why is this 1?", Some("deadbeef"));
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
        let retained_tree = git_with_stdin(
            repo,
            &["mktree"],
            &format!(
                "100644 blob {}\tblob\n100644 blob {}\tcontext\n",
                expected.anchor.blob,
                blob("one\ntwo\nthree\nfour\n"),
            ),
        );
        let root = git_with_stdin(
            repo,
            &["mktree"],
            &format!(
                "100644 blob {}\tbody\n\
                 040000 tree {anchor_tree}\tanchor\n\
                 040000 tree {issue_tree}\tissue\n\
                 040000 tree {retained_tree}\tretained\n",
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
