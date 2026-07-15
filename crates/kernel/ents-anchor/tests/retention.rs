//! Integration tests for `anchor.retention`: the serialized anchor embeds
//! the anchored blob and context blob as ordinary tree entries — reachable
//! from the storing document's tree, reproducing the original blob's object
//! id by content addressing, and never via a gitlink.

#![allow(clippy::unwrap_used, clippy::expect_used, reason = "integration test")]

use std::process::Command;

use ents_anchor::LineRange;
use facet_git_tree::{EntryKind, ObjectStore, RawTree, serialize_into};

/// A stand-in for `ents-forge`'s `Comment` (this crate cannot depend
/// on `ents-forge`, which itself depends on this crate): any struct
/// embedding an anchor's tree by [`RawTree`] exercises the same
/// reachability property `anchor.retention` requires.
#[derive(facet::Facet)]
struct Comment {
    body: String,
    anchor: RawTree,
}

fn fixture_repo(content: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["-c", "user.name=test", "-c", "user.email=test@example.com"])
            .args(args)
            .status()
            .unwrap();
        assert!(status.success());
    };
    run(&["init", "-q"]);
    std::fs::write(dir.path().join("file.txt"), content).unwrap();
    run(&["add", "-A"]);
    run(&["commit", "-q", "-m", "one"]);
    dir
}

fn numbered(range: std::ops::RangeInclusive<u32>) -> String {
    range.map(|n| format!("line {n}\n")).collect()
}

/// The serialized anchor's `content` and `context` entries are blobs — mode
/// `100644`, never a gitlink (`160000`) — and `content`'s object id is the
/// anchored blob's own id: referenced by content addressing, not copied
/// under a new identity.
// @relation(anchor.retention, scope=function, role=Verifies)
#[test]
fn retention_embeds_blobs_by_the_original_object_id_and_never_a_gitlink() {
    let dir = fixture_repo(&numbered(1..=10));
    let repo = gix::open(dir.path()).unwrap();
    let anchor = ents_anchor::capture(
        &repo,
        "HEAD",
        "file.txt",
        Some(LineRange { start: 3, end: 4 }),
    )
    .unwrap();

    let store = ObjectStore::default();
    let root = serialize_into(&anchor, &store).expect("serialize");
    let entries = store.get_tree(&root).expect("anchor tree");

    for entry in &entries {
        assert_ne!(
            entry.mode.kind(),
            EntryKind::Commit,
            "a gitlink retains nothing (anchor.retention): {:?}",
            entry.filename
        );
    }
    let content = entries
        .iter()
        .find(|e| e.filename == "content")
        .expect("content entry");
    assert_eq!(content.mode.kind(), EntryKind::Blob);
    assert_eq!(
        content.oid,
        anchor.blob(),
        "content addressing must reproduce the anchored blob's own id"
    );
    let context = entries
        .iter()
        .find(|e| e.filename == "context")
        .expect("context entry");
    assert_eq!(context.mode.kind(), EntryKind::Blob);
}

/// The anchored content stays reachable from the storing document's own
/// tree: walking the comment's tree (the shape `refs/meta/comments/*`
/// points at) reaches the anchored blob, so the ref keeps it alive through
/// force-push, branch deletion, and gc with no special-casing.
// @relation(anchor.retention, scope=function, role=Verifies)
#[test]
fn anchored_content_is_reachable_from_the_storing_documents_tree() {
    let dir = fixture_repo(&numbered(1..=10));
    let repo = gix::open(dir.path()).unwrap();
    let anchor = ents_anchor::capture(&repo, "HEAD", "file.txt", None).unwrap();

    let store = ObjectStore::default();
    let anchor_tree = serialize_into(&anchor, &store).expect("serialize anchor");
    let comment = Comment {
        body: "anchored".to_owned(),
        anchor: RawTree::new(anchor_tree),
    };
    let root = serialize_into(&comment, &store).expect("serialize comment");

    // Walk every tree reachable from the comment root; the anchored blob
    // must be among the reachable objects.
    let mut stack = vec![root];
    let mut found = false;
    while let Some(tree) = stack.pop() {
        for entry in store.get_tree(&tree).expect("tree") {
            match entry.mode.kind() {
                EntryKind::Tree => stack.push(entry.oid),
                _ => {
                    if entry.oid == anchor.blob() {
                        found = true;
                    }
                }
            }
        }
    }
    assert!(
        found,
        "the anchored blob must be reachable from the comment's own tree"
    );
}

/// A captured anchor round-trips through its tree unchanged — the struct is
/// the schema, and the retained bytes survive storage verbatim, non-ASCII
/// included.
// @relation(anchor.retention, scope=function, role=Verifies)
#[test]
fn anchor_round_trips_through_its_tree() {
    let dir = fixture_repo("line 1\nline 2\n\u{fe}\u{ff} non-ascii bytes\n");
    let repo = gix::open(dir.path()).unwrap();
    for lines in [None, Some(LineRange { start: 2, end: 3 })] {
        let anchor = ents_anchor::capture(&repo, "HEAD", "file.txt", lines).unwrap();
        let store = ObjectStore::default();
        let root = serialize_into(&anchor, &store).unwrap();
        let back: ents_anchor::Anchor = facet_git_tree::deserialize(&root, &store).unwrap();
        assert_eq!(back, anchor);
    }
}
