//! [`Anchor`] itself: identity, embedded retention, and capture.
//!
//! Spec coverage: `anchor.definition`, `anchor.immutable`, `anchor.retention`.

use facet::Facet;
use gix::ObjectId;
use gix::bstr::ByteSlice as _;

use crate::error::{Error, Result};
use crate::util::{lines_of, read_blob, resolve_commit};

/// A 1-based inclusive range of lines within an anchored file.
///
/// # Examples
///
/// ```
/// use ents_anchor::LineRange;
///
/// let range = LineRange { start: 3, end: 4 };
/// assert_eq!(range.end - range.start + 1, 2, "two lines, inclusive");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Facet)]
pub struct LineRange {
    /// The first line of the range, 1-based.
    pub start: u64,
    /// The last line of the range, inclusive.
    pub end: u64,
}

/// How many lines of surrounding source [`capture`] retains on each side of
/// an anchored range as `context` — enough for
/// [`crate::project_from_context`]'s line-window scan to recognize the
/// anchored lines' neighborhood even after they themselves moved a little,
/// without dragging in unrelated parts of a large file.
pub(crate) const CONTEXT_MARGIN: u64 = 3;

/// A durable pointer into source: authoritative at creation
/// (`anchor.immutable`) and never mutated afterward — every function that
/// takes one borrows it immutably, and projecting onto another commit
/// ([`crate::project`]) only ever produces a new [`crate::Projection`], never
/// a changed `Anchor`.
///
/// `commit` and `blob` identify exactly what was captured
/// (`anchor.definition`); `content` and `context` are the retained copies
/// (`anchor.retention`) that make the anchor durable — `content` is the
/// anchored blob's own bytes (so writing it into a store reproduces `blob`'s
/// object id exactly: content addressing makes "referenced rather than
/// copied" a fact about the bytes, not extra machinery), and `context` is a
/// small window around the anchored range, captured fresh, that
/// [`crate::project_from_context`] falls back to once `commit` itself has
/// been garbage collected. Neither is ever recomputed from the other after
/// capture: the anchored *text* ([`snippet`]) is always re-derived from
/// `content` and `lines` at read time rather than cached a third time.
///
/// `commit` is recorded on a best-effort basis only (`anchor.retention`):
/// nothing in this crate keeps it reachable, so it may already be gone by
/// the time the anchor is read back — that is exactly the case
/// [`crate::project_from_context`] exists for.
///
/// Serializing an `Anchor` (`facet_git_tree::serialize_into`) writes
/// `content` and `context` as ordinary blob tree entries alongside the
/// identity fields, in the same tree — never a gitlink, which names a commit
/// in another repository and would keep nothing reachable
/// (`anchor.retention`).
///
/// # Examples
///
/// ```
/// use ents_anchor::{Anchor, LineRange};
/// use facet_git_tree::{EntryKind, ObjectStore, serialize};
///
/// # fn write_numbered_file(dir: &std::path::Path) {
/// #     std::fs::write(dir.join("file.txt"), (1..=10).map(|n| format!("line {n}\n")).collect::<String>()).unwrap();
/// # }
/// # fn commit(dir: &std::path::Path) {
/// #     std::process::Command::new("git").arg("-C").arg(dir).args(["add", "-A"]).status().unwrap();
/// #     std::process::Command::new("git").arg("-C").arg(dir)
/// #         .args(["-c", "user.name=t", "-c", "user.email=t@example.com", "commit", "-q", "-m", "one"])
/// #         .status().unwrap();
/// # }
/// let dir = tempfile::tempdir().expect("tempdir");
/// std::process::Command::new("git").arg("init").arg("-q").arg(dir.path()).status().unwrap();
/// write_numbered_file(dir.path());
/// commit(dir.path());
///
/// let repo = gix::open(dir.path()).expect("open");
/// let anchor = ents_anchor::capture(&repo, "HEAD", "file.txt", Some(LineRange { start: 3, end: 4 }))
///     .expect("capture");
///
/// // The embedded content reproduces the exact anchored blob's own object
/// // id — "referenced ... rather than copied" (`anchor.retention`).
/// let (root, store) = serialize(&anchor).expect("serialize");
/// let (kind, oid) = {
///     let entries = store.get_tree(&root).expect("tree");
///     let entry = entries.iter().find(|e| e.filename == "content").expect("content entry");
///     (entry.mode.kind(), entry.oid)
/// };
/// assert_eq!(kind, EntryKind::Blob, "never a gitlink");
/// assert_eq!(oid, anchor.blob());
/// ```
// @relation(anchor.definition, anchor.immutable, anchor.retention, scope=file)
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct Anchor {
    pub(crate) commit: [u8; 20],
    /// The repository-relative path of the anchored file at `commit`.
    pub path: String,
    pub(crate) blob: [u8; 20],
    /// The anchored lines, or `None` for a whole-file anchor.
    pub lines: Option<LineRange>,
    /// The anchored blob's full bytes, embedded verbatim
    /// (`anchor.retention`) — reproduces [`Anchor::blob`]'s object id when
    /// written into any store, by content addressing.
    pub content: Vec<u8>,
    /// A window of up to `CONTEXT_MARGIN` (three) lines on either side of `lines`
    /// (or the whole file, for a whole-file anchor), captured fresh at
    /// [`capture`] time for [`crate::project_from_context`] to fuzzy-match
    /// against once `commit` is gone.
    pub context: Vec<u8>,
}

impl Anchor {
    /// The commit `self` was captured against, recorded on a best-effort
    /// basis: nothing keeps it reachable, so it may be gone (garbage
    /// collected) by the time the anchor is read back.
    /// [`crate::project_exact`] needs it to still exist;
    /// [`crate::project_from_context`] does not.
    #[must_use]
    pub fn commit(&self) -> ObjectId {
        ObjectId::from_bytes_or_panic(&self.commit)
    }

    /// The object id of the anchored file's blob at [`Anchor::commit`] — an
    /// integrity check and the fast path for "has this file changed at
    /// all".
    #[must_use]
    pub fn blob(&self) -> ObjectId {
        ObjectId::from_bytes_or_panic(&self.blob)
    }
}

/// Build the [`Anchor`] for `path` (and optionally `lines`) as it exists at
/// `revision` in `repo`, embedding the file's full content and a
/// `CONTEXT_MARGIN`-line (three-line) window around `lines`
/// (`anchor.retention`).
/// Fails when the path is not a file at that commit or the range does not
/// fit it (`anchor.definition`).
///
/// # Examples
///
/// ```
/// # let dir = tempfile::tempdir().expect("tempdir");
/// # std::process::Command::new("git").arg("init").arg("-q").arg(dir.path()).status().unwrap();
/// # std::fs::write(dir.path().join("file.txt"), "line 1\nline 2\nline 3\n").unwrap();
/// # std::process::Command::new("git").arg("-C").arg(dir.path()).args(["add", "-A"]).status().unwrap();
/// # std::process::Command::new("git").arg("-C").arg(dir.path())
/// #     .args(["-c", "user.name=t", "-c", "user.email=t@example.com", "commit", "-q", "-m", "one"])
/// #     .status().unwrap();
/// let repo = gix::open(dir.path()).expect("open");
/// let anchor = ents_anchor::capture(&repo, "HEAD", "file.txt", None).expect("capture");
/// assert_eq!(anchor.path, "file.txt");
/// assert_eq!(ents_anchor::snippet(&anchor).unwrap(), "line 1\nline 2\nline 3\n");
/// ```
// @relation(anchor.definition, anchor.retention, scope=function)
pub fn capture(
    repo: &gix::Repository,
    revision: &str,
    path: &str,
    lines: Option<LineRange>,
) -> Result<Anchor> {
    let commit = resolve_commit(repo, revision)?;
    let commit_id = commit.id().detach();
    let tree = commit
        .tree()
        .map_err(|error| Error::Object(error.to_string()))?;
    let entry = tree
        .lookup_entry_by_path(path)
        .map_err(|error| Error::Object(error.to_string()))?
        .filter(|entry| entry.mode().is_blob())
        .ok_or_else(|| Error::MissingPath {
            commit: commit_id,
            path: path.to_owned(),
        })?;
    let blob = entry.object_id();
    let content = read_blob(repo, blob)?;
    if let Some(range) = lines {
        lines_of(&content, path, range)?;
    }
    let context = capture_context(&content, lines);

    let mut commit_bytes = [0u8; 20];
    commit_bytes.copy_from_slice(commit_id.as_slice());
    let mut blob_bytes = [0u8; 20];
    blob_bytes.copy_from_slice(blob.as_slice());
    Ok(Anchor {
        commit: commit_bytes,
        path: path.to_owned(),
        blob: blob_bytes,
        lines,
        content,
        context,
    })
}

/// The exact text of `anchor`'s lines — the whole file for a whole-file
/// anchor — derived at read time from [`Anchor::content`], so it can never
/// disagree with what was captured and is never itself stored
/// (`anchor.immutable`).
///
/// # Examples
///
/// ```
/// # let dir = tempfile::tempdir().expect("tempdir");
/// # std::process::Command::new("git").arg("init").arg("-q").arg(dir.path()).status().unwrap();
/// # std::fs::write(dir.path().join("file.txt"), "a\nb\nc\n").unwrap();
/// # std::process::Command::new("git").arg("-C").arg(dir.path()).args(["add", "-A"]).status().unwrap();
/// # std::process::Command::new("git").arg("-C").arg(dir.path())
/// #     .args(["-c", "user.name=t", "-c", "user.email=t@example.com", "commit", "-q", "-m", "one"])
/// #     .status().unwrap();
/// let repo = gix::open(dir.path()).expect("open");
/// let anchor = ents_anchor::capture(&repo, "HEAD", "file.txt", Some(ents_anchor::LineRange { start: 2, end: 2 }))
///     .expect("capture");
/// assert_eq!(ents_anchor::snippet(&anchor).unwrap(), "b\n");
/// ```
// @relation(anchor.immutable, scope=function)
pub fn snippet(anchor: &Anchor) -> Result<String> {
    match anchor.lines {
        None => Ok(String::from_utf8_lossy(&anchor.content).into_owned()),
        Some(range) => lines_of(&anchor.content, &anchor.path, range),
    }
}

/// The anchored range (or, for a whole-file anchor, the whole file) plus up
/// to [`CONTEXT_MARGIN`] lines on either side within `content` — a small,
/// independently-retainable snapshot of the anchor's surroundings for
/// [`crate::project_from_context`] to fuzzy-match once the anchor's commit
/// is gone.
fn capture_context(content: &[u8], lines: Option<LineRange>) -> Vec<u8> {
    let Some(range) = lines else {
        return content.to_vec();
    };
    let all: Vec<&[u8]> = content.lines_with_terminator().collect();
    let len = u64::try_from(all.len()).unwrap_or(u64::MAX);
    let start0 = range.start.saturating_sub(1);
    let margin_before = CONTEXT_MARGIN.min(start0);
    let ctx_start = start0.saturating_sub(margin_before);
    let margin_after = CONTEXT_MARGIN.min(len.saturating_sub(range.end));
    let ctx_end = range.end.saturating_add(margin_after).min(len);
    let (Ok(ctx_start), Ok(ctx_end)) = (usize::try_from(ctx_start), usize::try_from(ctx_end))
    else {
        return Vec::new();
    };
    all.get(ctx_start..ctx_end).unwrap_or_default().concat()
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::panic,
        reason = "unit test; the panic is an assertion the type reflects as a struct at all"
    )]

    use facet::{Facet as _, Type, UserType};
    use rstest::rstest;

    use super::*;
    use crate::fixture::{commit_all, head, numbered, repo};

    fn range(start: u64, end: u64) -> Option<LineRange> {
        Some(LineRange { start, end })
    }

    // @relation(anchor.definition, scope=function, role=Verifies)
    #[test]
    fn capture_records_the_commit_and_blob_and_snippet_derives_the_text() {
        let dir = repo();
        std::fs::write(dir.path().join("file.txt"), numbered(1..=10)).unwrap();
        commit_all(dir.path(), "one");
        let git_repo = gix::open(dir.path()).unwrap();

        let anchor = capture(&git_repo, "HEAD", "file.txt", range(3, 4)).unwrap();
        assert_eq!(anchor.commit().to_string(), head(dir.path()));
        assert_eq!(anchor.path, "file.txt");
        assert_eq!(anchor.lines, range(3, 4));
        assert_eq!(anchor.content, numbered(1..=10).into_bytes());
        assert_eq!(snippet(&anchor).unwrap(), "line 3\nline 4\n");
    }

    #[rstest]
    #[case::missing_path("absent.txt", None)]
    #[case::oversized_range("file.txt", range(2, 9))]
    // @relation(anchor.definition, scope=function, role=Verifies)
    fn capture_rejects_a_missing_path_and_an_oversized_range(
        #[case] path: &str,
        #[case] lines: Option<LineRange>,
    ) {
        let dir = repo();
        std::fs::write(dir.path().join("file.txt"), numbered(1..=3)).unwrap();
        commit_all(dir.path(), "one");
        let git_repo = gix::open(dir.path()).unwrap();

        let error = capture(&git_repo, "HEAD", path, lines).unwrap_err();
        assert!(matches!(
            error,
            Error::MissingPath { .. } | Error::LinesOutOfRange { .. }
        ));
    }

    // @relation(anchor.retention, scope=function, role=Verifies)
    #[test]
    fn context_captures_a_margin_around_the_anchored_range() {
        let dir = repo();
        std::fs::write(dir.path().join("file.txt"), numbered(1..=10)).unwrap();
        commit_all(dir.path(), "one");
        let git_repo = gix::open(dir.path()).unwrap();
        let anchor = capture(&git_repo, "HEAD", "file.txt", range(5, 6)).unwrap();

        // 3 lines of margin on each side of a 2-line range: lines 2..=9.
        let expected: String = (2..=9).map(|n| format!("line {n}\n")).collect();
        assert_eq!(anchor.context, expected.into_bytes());
    }

    // @relation(anchor.retention, scope=function, role=Verifies)
    #[test]
    fn context_clamps_to_the_file_when_the_margin_would_overrun_it() {
        let dir = repo();
        std::fs::write(dir.path().join("file.txt"), numbered(1..=4)).unwrap();
        commit_all(dir.path(), "one");
        let git_repo = gix::open(dir.path()).unwrap();
        let anchor = capture(&git_repo, "HEAD", "file.txt", range(1, 2)).unwrap();

        assert_eq!(anchor.context, numbered(1..=4).into_bytes());
    }

    // @relation(anchor.retention, scope=function, role=Verifies)
    #[test]
    fn context_of_a_whole_file_anchor_is_the_whole_file() {
        let dir = repo();
        std::fs::write(dir.path().join("file.txt"), numbered(1..=5)).unwrap();
        commit_all(dir.path(), "one");
        let git_repo = gix::open(dir.path()).unwrap();
        let anchor = capture(&git_repo, "HEAD", "file.txt", None).unwrap();

        assert_eq!(anchor.context, numbered(1..=5).into_bytes());
    }

    // @relation(anchor.immutable, scope=function, role=Verifies)
    #[test]
    fn snippet_derives_text_from_content_and_never_stores_it_separately() {
        let Type::User(UserType::Struct(struct_ty)) = Anchor::SHAPE.ty else {
            panic!("Anchor must reflect as a struct");
        };
        let names: Vec<_> = struct_ty.fields.iter().map(|f| f.name).collect();
        assert_eq!(
            names,
            vec!["commit", "path", "blob", "lines", "content", "context"],
            "Anchor must derive its snippet from `content`, never cache it in a separate field"
        );
    }
}
