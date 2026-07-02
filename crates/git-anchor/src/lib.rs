//! Blob-anchored positions and their forward projection onto later commits.
//!
//! An [`Anchor`] records exactly where in a repository something (a comment, a
//! review note) was attached: the commit it was written against, the path and
//! blob at that commit, and an optional line range. The anchored text is never
//! stored — the blob is content-addressed, so [`snippet`] derives it exactly
//! at read time. The anchor is authoritative at creation and never mutated.
//! [`project`]
//! answers, at read time, where that position sits on any *other* commit —
//! following renames through git's rewrite tracking and shifting line ranges
//! through the blob's diff hunks, the way git itself re-derives positions when
//! replaying diffs on rebase.
//!
//! Projection is a two-point tree diff, not a history walk: it compares the
//! anchor commit's tree directly against the target commit's tree, so it works
//! whether the target is a descendant, an ancestor, or an unrelated commit.
//! Blame answers the backwards question (which commit introduced a line); the
//! forward question asked here needs only the diff.

use std::path::Path;

use facet::Facet;
use gix::ObjectId;
use gix::bstr::ByteSlice as _;
use gix::diff::blob::{Algorithm, Diff, InternedInput};
use gix::diff::tree_with_rewrites::Change;

/// A failure opening the repository or resolving the objects an anchor names.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The repository could not be opened.
    #[error("could not open the repository")]
    Open(#[from] Box<gix::open::Error>),
    /// A revision or object id could not be resolved to the object it names.
    #[error("could not resolve {0:?}")]
    Resolve(String),
    /// A git object could not be read.
    #[error("git object operation failed: {0}")]
    Object(String),
    /// The tree diff between the anchor commit and the target commit failed.
    #[error("tree diff failed: {0}")]
    Diff(String),
    /// The anchor names a path that is not a file in its commit.
    #[error("no file at {path:?} in {commit}")]
    MissingPath {
        /// The commit the path was looked up in.
        commit: String,
        /// The path that is not a file there.
        path: String,
    },
    /// The line range does not fit the file it is anchored to.
    #[error("lines {start}..={end} do not fit {path:?} ({len} lines)")]
    LinesOutOfRange {
        /// The file the range was checked against.
        path: String,
        /// The 1-based first line of the range.
        start: u64,
        /// The 1-based last line of the range.
        end: u64,
        /// How many lines the file actually has.
        len: u64,
    },
}

/// A 1-based inclusive range of lines within an anchored file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Facet)]
pub struct LineRange {
    /// The first line of the range, 1-based.
    pub start: u64,
    /// The last line of the range, inclusive.
    pub end: u64,
}

/// Where in the repository something was attached — authoritative at creation
/// and never mutated afterwards. Projection onto other commits is always a
/// read-time view derived from this record.
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct Anchor {
    /// The commit the anchor was created against, as a hex object id.
    pub commit: String,
    /// The repository-relative path of the anchored file at that commit.
    pub path: String,
    /// The object id of the anchored file's blob, as hex — an integrity check
    /// and the fast path for "has this file changed at all".
    pub blob: String,
    /// The anchored lines, or `None` for a whole-file anchor.
    pub lines: Option<LineRange>,
}

/// Where an [`Anchor`] sits on a target commit, as computed by [`project`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Projection {
    /// The target tree holds the anchor's exact blob at its exact path; the
    /// anchor applies unchanged.
    Current,
    /// The file moved and/or its content shifted, but the anchored region
    /// itself is intact — the anchor now applies at `path` and `lines`.
    Relocated {
        /// The anchored file's path in the target tree.
        path: String,
        /// The anchored lines mapped into the target blob, or `None` for a
        /// whole-file anchor.
        lines: Option<LineRange>,
    },
    /// The file survives at `path` but the anchored lines were edited (or the
    /// entry is no longer a regular file); the anchor no longer maps cleanly.
    Outdated {
        /// The anchored file's path in the target tree.
        path: String,
    },
    /// The anchored file does not exist in the target tree.
    FileDeleted,
}

/// Build the [`Anchor`] for `path` (and optionally `lines`) as it exists at
/// `revision` in `repo`, resolving the revision to a full commit id and
/// recording the file's blob id. Fails when the path is not a file at that
/// commit or the range does not fit it.
pub fn capture(
    repo: &Path,
    revision: &str,
    path: &str,
    lines: Option<LineRange>,
) -> Result<Anchor, Error> {
    let repo = gix::open(repo).map_err(|error| Error::Open(Box::new(error)))?;
    let commit = resolve_commit(&repo, revision)?;
    let commit_id = commit.id().to_string();
    let tree = commit
        .tree()
        .map_err(|error| Error::Object(error.to_string()))?;
    let entry = tree
        .lookup_entry_by_path(path)
        .map_err(|error| Error::Object(error.to_string()))?
        .filter(|entry| entry.mode().is_blob())
        .ok_or_else(|| Error::MissingPath {
            commit: commit_id.clone(),
            path: path.to_owned(),
        })?;
    let blob = entry.object_id();
    if let Some(range) = lines {
        let data = read_blob(&repo, blob)?;
        lines_of(&data, path, range)?;
    }
    Ok(Anchor {
        commit: commit_id,
        path: path.to_owned(),
        blob: blob.to_string(),
        lines,
    })
}

/// The exact text of `anchor`'s lines — the whole file for a whole-file
/// anchor — derived at read time from the content-addressed blob the anchor
/// names, so it can never disagree with what was anchored.
pub fn snippet(repo: &Path, anchor: &Anchor) -> Result<String, Error> {
    let repo = gix::open(repo).map_err(|error| Error::Open(Box::new(error)))?;
    let blob = ObjectId::from_hex(anchor.blob.as_bytes())
        .map_err(|_error| Error::Resolve(anchor.blob.clone()))?;
    let data = read_blob(&repo, blob)?;
    match anchor.lines {
        None => Ok(String::from_utf8_lossy(&data).into_owned()),
        Some(range) => lines_of(&data, &anchor.path, range),
    }
}

/// The text of the 1-based inclusive `range` within `data`, or
/// [`Error::LinesOutOfRange`] (naming `path`) when the range does not fit.
fn lines_of(data: &[u8], path: &str, range: LineRange) -> Result<String, Error> {
    let all: Vec<&[u8]> = data.lines_with_terminator().collect();
    let out_of_range = || Error::LinesOutOfRange {
        path: path.to_owned(),
        start: range.start,
        end: range.end,
        len: u64::try_from(all.len()).unwrap_or(u64::MAX),
    };
    // One slice lookup validates the whole range: start == 0 dies in
    // checked_sub, an inverted or oversized range dies in get.
    let first = usize::try_from(range.start)
        .ok()
        .and_then(|start| start.checked_sub(1))
        .ok_or_else(out_of_range)?;
    let last = usize::try_from(range.end).ok().ok_or_else(out_of_range)?;
    let bytes = all.get(first..last).ok_or_else(out_of_range)?.concat();
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Project `anchor` onto `target` (a revision in `repo`): the fast path
/// returns [`Projection::Current`] when the target tree holds the anchor's
/// blob at its path; otherwise the anchor commit's tree is diffed against the
/// target's with rename tracking to find where the file went, and the line
/// range is mapped through the blob diff's hunks — shifted past edits that
/// land entirely outside it, [`Projection::Outdated`] when an edit touches it.
pub fn project(repo: &Path, anchor: &Anchor, target: &str) -> Result<Projection, Error> {
    let repo = gix::open(repo).map_err(|error| Error::Open(Box::new(error)))?;
    let anchor_blob = ObjectId::from_hex(anchor.blob.as_bytes())
        .map_err(|_error| Error::Resolve(anchor.blob.clone()))?;
    let target_commit = resolve_commit(&repo, target)?;
    let target_tree = target_commit
        .tree()
        .map_err(|error| Error::Object(error.to_string()))?;

    if let Some(entry) = target_tree
        .lookup_entry_by_path(&anchor.path)
        .map_err(|error| Error::Object(error.to_string()))?
        && entry.mode().is_blob()
        && entry.object_id() == anchor_blob
    {
        return Ok(Projection::Current);
    }

    let anchor_commit = resolve_commit(&repo, &anchor.commit)?;
    let anchor_tree = anchor_commit
        .tree()
        .map_err(|error| Error::Object(error.to_string()))?;
    // Rename tracking is pinned to git's defaults (50% similarity, no copies)
    // rather than read from repository configuration, so a projection is the
    // same answer everywhere the repository is checked out.
    let options = gix::diff::Options::default().with_rewrites(Some(gix::diff::Rewrites::default()));
    let changes = repo
        .diff_tree_to_tree(Some(&anchor_tree), Some(&target_tree), options)
        .map_err(|error| Error::Diff(error.to_string()))?;

    // Find where the anchored path went: its old-side location is `location`
    // for a deletion or modification and `source_location` for a rename.
    let mut destination: Option<(String, ObjectId, bool)> = None;
    for change in changes {
        match change {
            Change::Deletion { location, .. } if location.as_bytes() == anchor.path.as_bytes() => {
                return Ok(Projection::FileDeleted);
            }
            Change::Modification {
                location,
                id,
                entry_mode,
                ..
            } if location.as_bytes() == anchor.path.as_bytes() => {
                destination = Some((anchor.path.clone(), id, entry_mode.is_blob()));
                break;
            }
            Change::Rewrite {
                source_location,
                location,
                id,
                entry_mode,
                copy: false,
                ..
            } if source_location.as_bytes() == anchor.path.as_bytes() => {
                destination = Some((
                    location.to_str_lossy().into_owned(),
                    id,
                    entry_mode.is_blob(),
                ));
                break;
            }
            _ => {}
        }
    }
    let Some((path, blob, is_blob)) = destination else {
        // The diff never touched the path, yet the fast path did not match:
        // the anchor's blob is not what its own commit holds there, so the
        // anchor itself is broken.
        return Err(Error::MissingPath {
            commit: anchor.commit.clone(),
            path: anchor.path.clone(),
        });
    };
    if !is_blob {
        return Ok(Projection::Outdated { path });
    }
    if blob == anchor_blob {
        // A pure rename: the content is byte-identical, so every line is
        // exactly where it was.
        return Ok(Projection::Relocated {
            path,
            lines: anchor.lines,
        });
    }
    let lines = match anchor.lines {
        None => None,
        Some(range) => {
            let old = read_blob(&repo, anchor_blob)?;
            let new = read_blob(&repo, blob)?;
            match map_range(&old, &new, range) {
                Some(mapped) => Some(mapped),
                None => return Ok(Projection::Outdated { path }),
            }
        }
    };
    Ok(Projection::Relocated { path, lines })
}

/// Resolve `revision` (a hex id, ref name, or revspec) to the commit it names.
fn resolve_commit<'repo>(
    repo: &'repo gix::Repository,
    revision: &str,
) -> Result<gix::Commit<'repo>, Error> {
    let resolve = || Error::Resolve(revision.to_owned());
    repo.rev_parse_single(revision)
        .map_err(|_error| resolve())?
        .object()
        .map_err(|_error| resolve())?
        .peel_to_kind(gix::object::Kind::Commit)
        .map_err(|_error| resolve())?
        .try_into_commit()
        .map_err(|_error| resolve())
}

/// Read the full contents of the blob at `id`.
fn read_blob(repo: &gix::Repository, id: ObjectId) -> Result<Vec<u8>, Error> {
    Ok(repo
        .find_blob(id)
        .map_err(|error| Error::Object(error.to_string()))?
        .take_data())
}

/// Map the 1-based inclusive `range` from `old`'s lines to `new`'s by walking
/// the diff's hunks in order: a hunk entirely above the range shifts it by
/// the hunk's growth, a hunk entirely below is ignored, and any hunk touching
/// the range — including an insertion strictly inside it — means the anchored
/// region itself changed, reported as `None` (outdated) rather than guessed
/// at.
fn map_range(old: &[u8], new: &[u8], range: LineRange) -> Option<LineRange> {
    // Work in 0-based half-open line coordinates, as the hunks do. Everything
    // stays unsigned: the shift is tallied as lines added and lines removed
    // above the range, and any overflow is an honest `None` (outdated) via the
    // checked arithmetic rather than a saturated wrong answer.
    let start = range.start.checked_sub(1)?;
    let end = range.end;
    if end <= start {
        return None;
    }
    let input = InternedInput::new(old, new);
    if end > u64::try_from(input.before.len()).ok()? {
        return None;
    }
    let diff = Diff::compute(Algorithm::Histogram, &input);
    let mut added: u64 = 0;
    let mut removed: u64 = 0;
    for hunk in diff.hunks() {
        let before_start = u64::from(hunk.before.start);
        let before_end = u64::from(hunk.before.end);
        if before_end <= start {
            removed = removed.checked_add(before_end.checked_sub(before_start)?)?;
            added = added
                .checked_add(u64::from(hunk.after.end).checked_sub(u64::from(hunk.after.start))?)?;
        } else if before_start >= end {
            break;
        } else {
            return None;
        }
    }
    let map = |line: u64| line.checked_add(added)?.checked_sub(removed);
    Some(LineRange {
        start: map(start)?.checked_add(1)?,
        end: map(end)?,
    })
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

    use super::*;

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let status = Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["init", "-q"])
            .status()
            .unwrap();
        assert!(status.success());
        dir
    }

    fn commit_all(dir: &Path, message: &str) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["add", "-A"])
            .status()
            .unwrap();
        assert!(status.success());
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-q",
                "-m",
                message,
            ])
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn head(dir: &Path) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn numbered(range: std::ops::RangeInclusive<u32>) -> String {
        range.map(|n| format!("line {n}\n")).collect()
    }

    fn range(start: u64, end: u64) -> Option<LineRange> {
        Some(LineRange { start, end })
    }

    #[test]
    fn capture_records_the_commit_and_blob_and_snippet_derives_the_text() {
        let dir = repo();
        std::fs::write(dir.path().join("file.txt"), numbered(1..=10)).unwrap();
        commit_all(dir.path(), "one");

        let anchor = capture(dir.path(), "HEAD", "file.txt", range(3, 4)).unwrap();
        assert_eq!(anchor.commit, head(dir.path()));
        assert_eq!(anchor.path, "file.txt");
        assert_eq!(anchor.lines, range(3, 4));
        assert!(!anchor.blob.is_empty());
        assert_eq!(snippet(dir.path(), &anchor).unwrap(), "line 3\nline 4\n");
    }

    #[test]
    fn capture_rejects_a_missing_path_and_an_oversized_range() {
        let dir = repo();
        std::fs::write(dir.path().join("file.txt"), numbered(1..=3)).unwrap();
        commit_all(dir.path(), "one");

        assert!(matches!(
            capture(dir.path(), "HEAD", "absent.txt", None),
            Err(Error::MissingPath { .. })
        ));
        assert!(matches!(
            capture(dir.path(), "HEAD", "file.txt", range(2, 9)),
            Err(Error::LinesOutOfRange { len: 3, .. })
        ));
    }

    #[test]
    fn unchanged_file_projects_as_current() {
        let dir = repo();
        std::fs::write(dir.path().join("file.txt"), numbered(1..=10)).unwrap();
        commit_all(dir.path(), "one");
        let anchor = capture(dir.path(), "HEAD", "file.txt", range(3, 4)).unwrap();

        std::fs::write(dir.path().join("other.txt"), "unrelated\n").unwrap();
        commit_all(dir.path(), "two");

        assert_eq!(
            project(dir.path(), &anchor, "HEAD").unwrap(),
            Projection::Current
        );
    }

    #[test]
    fn an_edit_above_the_range_shifts_it() {
        let dir = repo();
        std::fs::write(dir.path().join("file.txt"), numbered(1..=10)).unwrap();
        commit_all(dir.path(), "one");
        let anchor = capture(dir.path(), "HEAD", "file.txt", range(5, 6)).unwrap();

        let edited = format!("added a\nadded b\n{}", numbered(1..=10));
        std::fs::write(dir.path().join("file.txt"), edited).unwrap();
        commit_all(dir.path(), "two");

        assert_eq!(
            project(dir.path(), &anchor, "HEAD").unwrap(),
            Projection::Relocated {
                path: "file.txt".to_owned(),
                lines: range(7, 8),
            }
        );
    }

    #[test]
    fn an_edit_inside_the_range_is_outdated() {
        let dir = repo();
        std::fs::write(dir.path().join("file.txt"), numbered(1..=10)).unwrap();
        commit_all(dir.path(), "one");
        let anchor = capture(dir.path(), "HEAD", "file.txt", range(5, 6)).unwrap();

        let edited = numbered(1..=10).replace("line 5\n", "line five\n");
        std::fs::write(dir.path().join("file.txt"), edited).unwrap();
        commit_all(dir.path(), "two");

        assert_eq!(
            project(dir.path(), &anchor, "HEAD").unwrap(),
            Projection::Outdated {
                path: "file.txt".to_owned(),
            }
        );
    }

    #[test]
    fn a_pure_rename_relocates_with_the_same_lines() {
        let dir = repo();
        std::fs::write(dir.path().join("file.txt"), numbered(1..=10)).unwrap();
        commit_all(dir.path(), "one");
        let anchor = capture(dir.path(), "HEAD", "file.txt", range(3, 4)).unwrap();

        std::fs::rename(dir.path().join("file.txt"), dir.path().join("moved.txt")).unwrap();
        commit_all(dir.path(), "two");

        assert_eq!(
            project(dir.path(), &anchor, "HEAD").unwrap(),
            Projection::Relocated {
                path: "moved.txt".to_owned(),
                lines: range(3, 4),
            }
        );
    }

    #[test]
    fn a_rename_with_an_edit_above_relocates_and_shifts() {
        let dir = repo();
        std::fs::write(dir.path().join("file.txt"), numbered(1..=10)).unwrap();
        commit_all(dir.path(), "one");
        let anchor = capture(dir.path(), "HEAD", "file.txt", range(5, 6)).unwrap();

        std::fs::remove_file(dir.path().join("file.txt")).unwrap();
        let edited = format!("added a\n{}", numbered(1..=10));
        std::fs::write(dir.path().join("moved.txt"), edited).unwrap();
        commit_all(dir.path(), "two");

        assert_eq!(
            project(dir.path(), &anchor, "HEAD").unwrap(),
            Projection::Relocated {
                path: "moved.txt".to_owned(),
                lines: range(6, 7),
            }
        );
    }

    #[test]
    fn a_deleted_file_projects_as_deleted() {
        let dir = repo();
        std::fs::write(dir.path().join("file.txt"), numbered(1..=10)).unwrap();
        commit_all(dir.path(), "one");
        let anchor = capture(dir.path(), "HEAD", "file.txt", range(3, 4)).unwrap();

        std::fs::remove_file(dir.path().join("file.txt")).unwrap();
        std::fs::write(dir.path().join("unrelated.txt"), "different content\n").unwrap();
        commit_all(dir.path(), "two");

        assert_eq!(
            project(dir.path(), &anchor, "HEAD").unwrap(),
            Projection::FileDeleted
        );
    }

    #[test]
    fn a_whole_file_anchor_survives_a_modification() {
        let dir = repo();
        std::fs::write(dir.path().join("file.txt"), numbered(1..=10)).unwrap();
        commit_all(dir.path(), "one");
        let anchor = capture(dir.path(), "HEAD", "file.txt", None).unwrap();
        assert_eq!(snippet(dir.path(), &anchor).unwrap(), numbered(1..=10));

        let edited = numbered(1..=10).replace("line 5\n", "line five\n");
        std::fs::write(dir.path().join("file.txt"), edited).unwrap();
        commit_all(dir.path(), "two");

        assert_eq!(
            project(dir.path(), &anchor, "HEAD").unwrap(),
            Projection::Relocated {
                path: "file.txt".to_owned(),
                lines: None,
            }
        );
    }

    #[test]
    fn projection_works_backwards_onto_an_ancestor() {
        let dir = repo();
        std::fs::write(dir.path().join("file.txt"), numbered(1..=10)).unwrap();
        commit_all(dir.path(), "one");
        let old = head(dir.path());

        let edited = format!("added a\n{}", numbered(1..=10));
        std::fs::write(dir.path().join("file.txt"), edited).unwrap();
        commit_all(dir.path(), "two");
        let anchor = capture(dir.path(), "HEAD", "file.txt", range(6, 7)).unwrap();

        assert_eq!(
            project(dir.path(), &anchor, &old).unwrap(),
            Projection::Relocated {
                path: "file.txt".to_owned(),
                lines: range(5, 6),
            }
        );
    }

    #[test]
    fn map_range_handles_edges() {
        let old = b"a\nb\nc\nd\n".as_slice();
        // An insertion exactly at the range start shifts it; one exactly at
        // its end leaves it alone.
        let above = b"x\na\nb\nc\nd\n".as_slice();
        assert_eq!(
            map_range(old, above, LineRange { start: 2, end: 3 }),
            Some(LineRange { start: 3, end: 4 })
        );
        // An insertion strictly inside the range outdates it.
        let inside = b"a\nb\nx\nc\nd\n".as_slice();
        assert_eq!(map_range(old, inside, LineRange { start: 2, end: 3 }), None);
        // A range past the end of the old file cannot map.
        assert_eq!(map_range(old, old, LineRange { start: 4, end: 9 }), None);
    }
}
