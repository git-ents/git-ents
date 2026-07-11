//! Read-time projection of an [`Anchor`](crate::Anchor) onto another
//! commit: an exact tree diff when the anchor's own commit still exists,
//! degrading to fuzzy context matching once it is gone.
//!
//! Spec coverage: `anchor.projection`, `anchor.fuzzy-fallback`.
//!
//! Projection is a two-point tree diff, not a history walk: [`project_exact`]
//! compares the anchor commit's tree directly against the target commit's
//! tree, so it works whether the target is a descendant, an ancestor, or
//! unrelated history. Blame answers the backwards question (which commit
//! introduced a line); the forward question asked here needs only the diff.

use gix::ObjectId;
use gix::bstr::ByteSlice as _;
use gix::diff::blob::{Algorithm, Diff, InternedInput};
use gix::diff::tree_with_rewrites::Change;

use crate::anchor::{Anchor, CONTEXT_MARGIN, LineRange};
use crate::error::{Error, Result};
use crate::util::{commit_at, read_blob, resolve_commit};

/// Where an [`Anchor`] sits on a target commit, as computed by [`project`].
///
/// # Examples
///
/// ```
/// use ents_anchor::Projection;
///
/// let outcome = Projection::Outdated { path: "src/lib.rs".to_owned() };
/// assert!(matches!(outcome, Projection::Outdated { .. }));
/// ```
// @relation(anchor.projection, scope=file)
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
    /// The file survives at `path` but the anchored lines were edited (or
    /// the entry is no longer a regular file); the anchor no longer maps
    /// cleanly.
    Outdated {
        /// The anchored file's path in the target tree.
        path: String,
    },
    /// The anchored file does not exist in the target tree.
    Deleted,
}

/// Project `anchor` onto `target` (a revision in `repo`), degrading to
/// [`project_from_context`] once `anchor`'s own commit has been garbage
/// collected (`anchor.fuzzy-fallback`) — the one entry point most callers
/// need; [`project_exact`] and [`project_from_context`] are exposed
/// separately for callers that need to distinguish an exact projection from
/// an approximate one.
///
/// Never mutates `anchor`: every outcome, including [`Projection::Outdated`]
/// and [`Projection::Deleted`], is a fresh [`Projection`] value, and the
/// anchor itself remains displayable regardless of the outcome
/// (`anchor.fuzzy-fallback`).
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
/// let anchor = ents_anchor::capture(&repo, "HEAD", "file.txt", None).expect("capture");
/// assert_eq!(ents_anchor::project(&repo, &anchor, "HEAD").unwrap(), ents_anchor::Projection::Current);
/// ```
// @relation(anchor.projection, anchor.fuzzy-fallback, scope=function)
pub fn project(repo: &gix::Repository, anchor: &Anchor, target: &str) -> Result<Projection> {
    match project_exact(repo, anchor, target) {
        Err(Error::AnchorCommitMissing(_)) => project_from_context(repo, anchor, target),
        other => other,
    }
}

/// Project `anchor` onto `target` by diffing `anchor`'s own commit tree
/// against `target`'s, with rename tracking, and mapping the line range
/// through the blob diff's hunks — shifted past edits that land entirely
/// outside it, [`Projection::Outdated`] when an edit touches it.
///
/// Fails with [`Error::AnchorCommitMissing`] when `anchor`'s commit no
/// longer exists (it is retained on a best-effort basis only,
/// `anchor.retention`); [`project`] catches exactly this and retries with
/// [`project_from_context`], which needs no commit at all.
// @relation(anchor.projection, scope=function)
pub fn project_exact(repo: &gix::Repository, anchor: &Anchor, target: &str) -> Result<Projection> {
    let anchor_blob = anchor.blob();
    let anchor_commit_id = anchor.commit();
    let target_commit = resolve_commit(repo, target)?;
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

    if !repo.has_object(anchor_commit_id) {
        return Err(Error::AnchorCommitMissing(anchor_commit_id));
    }
    let anchor_commit = commit_at(repo, anchor_commit_id)?;
    let anchor_tree = anchor_commit
        .tree()
        .map_err(|error| Error::Object(error.to_string()))?;
    // Rename tracking is pinned to git's defaults (50% similarity, no
    // copies) rather than read from repository configuration, so a
    // projection is the same answer everywhere the repository is checked
    // out.
    let options = gix::diff::Options::default().with_rewrites(Some(gix::diff::Rewrites::default()));
    let changes = repo
        .diff_tree_to_tree(Some(&anchor_tree), Some(&target_tree), options)
        .map_err(|error| Error::Diff(error.to_string()))?;

    // Find where the anchored path went: its old-side location is
    // `location` for a deletion or modification and `source_location` for a
    // rename.
    let mut destination: Option<(String, ObjectId, bool)> = None;
    for change in changes {
        match change {
            Change::Deletion { location, .. } if location.as_bytes() == anchor.path.as_bytes() => {
                return Ok(Projection::Deleted);
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
        // The diff never touched the path, yet the fast path did not
        // match: the anchor's blob is not what its own commit holds there,
        // so the anchor itself is broken.
        return Err(Error::MissingPath {
            commit: anchor_commit_id,
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
            let new = read_blob(repo, blob)?;
            match map_range(&anchor.content, &new, range) {
                Some(mapped) => Some(mapped),
                None => return Ok(Projection::Outdated { path }),
            }
        }
    };
    Ok(Projection::Relocated { path, lines })
}

/// Project `anchor` onto `target` by fuzzy-matching `anchor`'s retained
/// `context` (`anchor.retention`) against `target`'s version of
/// `anchor.path`, for use once `anchor`'s commit no longer exists and
/// [`project_exact`] can no longer diff against its tree.
///
/// Looks up `anchor.path` in `target`'s tree directly (no rename tracking is
/// possible without the anchor commit's tree, so a genuine rename reports
/// [`Projection::Deleted`] here, same as a real deletion); a whole-file
/// anchor (`anchor.lines` is `None`) survives any edit at that path, same as
/// [`project_exact`]. For a line-range anchor, every contiguous window of
/// the target file's lines the same length as `context` is scored by how
/// many lines match `context` exactly; the best-scoring window (at least
/// half its lines matching) is accepted and the anchored sub-range is mapped
/// back through the same margin [`crate::capture`] used to build `context`.
/// No match clearing that bar reports [`Projection::Outdated`], the same as
/// an unrecoverable edit would under [`project_exact`].
// @relation(anchor.fuzzy-fallback, scope=function)
pub fn project_from_context(
    repo: &gix::Repository,
    anchor: &Anchor,
    target: &str,
) -> Result<Projection> {
    let target_commit = resolve_commit(repo, target)?;
    let target_tree = target_commit
        .tree()
        .map_err(|error| Error::Object(error.to_string()))?;
    let Some(entry) = target_tree
        .lookup_entry_by_path(&anchor.path)
        .map_err(|error| Error::Object(error.to_string()))?
    else {
        return Ok(Projection::Deleted);
    };
    if !entry.mode().is_blob() {
        return Ok(Projection::Outdated {
            path: anchor.path.clone(),
        });
    }
    let Some(range) = anchor.lines else {
        return Ok(Projection::Relocated {
            path: anchor.path.clone(),
            lines: None,
        });
    };

    let data = read_blob(repo, entry.object_id())?;
    let target_lines: Vec<&[u8]> = data.lines_with_terminator().collect();
    let context_lines: Vec<&[u8]> = anchor.context.lines_with_terminator().collect();
    let window = context_lines.len();
    if window == 0 || window > target_lines.len() {
        return Ok(Projection::Outdated {
            path: anchor.path.clone(),
        });
    }

    let mut best: Option<(usize, usize)> = None;
    for (start, slice) in target_lines.windows(window).enumerate() {
        let score = slice
            .iter()
            .zip(context_lines.iter())
            .filter(|(have, want)| have == want)
            .count();
        if best.is_none_or(|(_start, best_score)| score > best_score) {
            best = Some((start, score));
        }
    }
    // Require at least half the window's lines to match exactly, so an
    // unrelated coincidence of blank or near-empty lines is not mistaken
    // for the anchored region having relocated there.
    let Some((start, _score)) = best.filter(|(_start, score)| {
        score
            .checked_mul(2)
            .is_some_and(|doubled| doubled >= window)
    }) else {
        return Ok(Projection::Outdated {
            path: anchor.path.clone(),
        });
    };

    let margin_before = CONTEXT_MARGIN.min(range.start.saturating_sub(1));
    let range_len = range.end.saturating_sub(range.start).saturating_add(1);
    let Ok(start) = u64::try_from(start) else {
        return Ok(Projection::Outdated {
            path: anchor.path.clone(),
        });
    };
    let mapped_start = start.saturating_add(margin_before).saturating_add(1);
    let mapped_end = mapped_start.saturating_add(range_len).saturating_sub(1);
    Ok(Projection::Relocated {
        path: anchor.path.clone(),
        lines: Some(LineRange {
            start: mapped_start,
            end: mapped_end,
        }),
    })
}

/// Map the 1-based inclusive `range` from `old`'s lines to `new`'s by
/// walking the diff's hunks in order: a hunk entirely above the range
/// shifts it by the hunk's growth, a hunk entirely below is ignored, and any
/// hunk touching the range — including an insertion strictly inside it —
/// means the anchored region itself changed, reported as `None` (outdated)
/// rather than guessed at.
// @relation(anchor.projection, scope=function)
fn map_range(old: &[u8], new: &[u8], range: LineRange) -> Option<LineRange> {
    // Work in 0-based half-open line coordinates, as the hunks do.
    // Everything stays unsigned: the shift is tallied as lines added and
    // lines removed above the range, and any overflow is an honest `None`
    // (outdated) via the checked arithmetic rather than a saturated wrong
    // answer.
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
        clippy::arithmetic_side_effects,
        reason = "unit test; property inputs are bounded well below overflow"
    )]

    use rstest::rstest;

    use super::*;
    use crate::anchor::capture;
    use crate::fixture::{commit_all, numbered, repo};

    fn range(start: u64, end: u64) -> Option<LineRange> {
        Some(LineRange { start, end })
    }

    /// One post-capture edit per taxonomy row of
    /// [`projection_reports_the_spec_outcomes`].
    #[derive(Debug, Clone, Copy)]
    enum Mutation {
        TouchOtherFile,
        PrependTwoLines,
        EditLineFive,
        Rename,
        RenameAndPrependOneLine,
        Delete,
    }

    impl Mutation {
        fn apply(self, dir: &std::path::Path) {
            let file = dir.join("file.txt");
            match self {
                Self::TouchOtherFile => {
                    std::fs::write(dir.join("other.txt"), "unrelated\n").unwrap();
                }
                Self::PrependTwoLines => {
                    std::fs::write(&file, format!("added a\nadded b\n{}", numbered(1..=10)))
                        .unwrap();
                }
                Self::EditLineFive => {
                    let edited = numbered(1..=10).replace("line 5\n", "line five\n");
                    std::fs::write(&file, edited).unwrap();
                }
                Self::Rename => {
                    std::fs::rename(&file, dir.join("moved.txt")).unwrap();
                }
                Self::RenameAndPrependOneLine => {
                    std::fs::remove_file(&file).unwrap();
                    std::fs::write(
                        dir.join("moved.txt"),
                        format!("added a\n{}", numbered(1..=10)),
                    )
                    .unwrap();
                }
                Self::Delete => {
                    std::fs::remove_file(&file).unwrap();
                    std::fs::write(dir.join("unrelated.txt"), "different content\n").unwrap();
                }
            }
        }
    }

    /// `anchor.projection`'s outcome taxonomy, enumerated over the
    /// scenarios that select each outcome: unchanged (current), an edit
    /// above the range (relocated: shifted), an edit inside the range
    /// (outdated), a pure rename (relocated: same lines), a rename with an
    /// edit above (relocated: new path and shifted lines), a deletion
    /// (deleted), and a whole-file anchor surviving a modification
    /// (relocated: no lines).
    #[rstest]
    #[case::unchanged_is_current(Mutation::TouchOtherFile, range(3, 4), Projection::Current)]
    #[case::edit_above_shifts(
        Mutation::PrependTwoLines,
        range(5, 6),
        Projection::Relocated { path: "file.txt".to_owned(), lines: range(7, 8) }
    )]
    #[case::edit_inside_outdates(
        Mutation::EditLineFive,
        range(5, 6),
        Projection::Outdated { path: "file.txt".to_owned() }
    )]
    #[case::pure_rename_relocates(
        Mutation::Rename,
        range(3, 4),
        Projection::Relocated { path: "moved.txt".to_owned(), lines: range(3, 4) }
    )]
    #[case::rename_with_edit_above(
        Mutation::RenameAndPrependOneLine,
        range(5, 6),
        Projection::Relocated { path: "moved.txt".to_owned(), lines: range(6, 7) }
    )]
    #[case::deletion_is_deleted(Mutation::Delete, range(3, 4), Projection::Deleted)]
    #[case::whole_file_survives_an_edit(
        Mutation::EditLineFive,
        None,
        Projection::Relocated { path: "file.txt".to_owned(), lines: None }
    )]
    // @relation(anchor.projection, scope=function, role=Verifies)
    fn projection_reports_the_spec_outcomes(
        #[case] mutation: Mutation,
        #[case] lines: Option<LineRange>,
        #[case] expected: Projection,
    ) {
        let dir = repo();
        std::fs::write(dir.path().join("file.txt"), numbered(1..=10)).unwrap();
        commit_all(dir.path(), "one");
        let git_repo = gix::open(dir.path()).unwrap();
        let anchor = capture(&git_repo, "HEAD", "file.txt", lines).unwrap();

        mutation.apply(dir.path());
        commit_all(dir.path(), "two");

        // Re-open: the first handle predates commit two.
        let git_repo = gix::open(dir.path()).unwrap();
        assert_eq!(project_exact(&git_repo, &anchor, "HEAD").unwrap(), expected);
        // The umbrella entry point gives the identical answer while the
        // anchor commit exists.
        assert_eq!(project(&git_repo, &anchor, "HEAD").unwrap(), expected);
    }

    // @relation(anchor.projection, scope=function, role=Verifies)
    #[test]
    fn projection_works_backwards_onto_an_ancestor() {
        let dir = repo();
        std::fs::write(dir.path().join("file.txt"), numbered(1..=10)).unwrap();
        commit_all(dir.path(), "one");
        let git_repo = gix::open(dir.path()).unwrap();
        let old = git_repo.head_id().unwrap().detach().to_string();

        let edited = format!("added a\n{}", numbered(1..=10));
        std::fs::write(dir.path().join("file.txt"), edited).unwrap();
        commit_all(dir.path(), "two");
        let git_repo = gix::open(dir.path()).unwrap();
        let anchor = capture(&git_repo, "HEAD", "file.txt", range(6, 7)).unwrap();

        assert_eq!(
            project_exact(&git_repo, &anchor, &old).unwrap(),
            Projection::Relocated {
                path: "file.txt".to_owned(),
                lines: range(5, 6),
            }
        );
    }

    proptest::proptest! {
        /// Projection stability under content perturbation
        /// (`anchor.projection`): inserting lines strictly above the
        /// anchored range shifts it by exactly the insertion count, and
        /// appending lines strictly below leaves it untouched — for any
        /// file size, range, and insertion size.
        // @relation(anchor.projection, scope=function, role=Verifies)
        #[test]
        fn map_range_shifts_past_outside_edits_and_only_outside_edits(
            file_len in 1u64..200,
            range_start in 1u64..200,
            range_len in 0u64..20,
            inserted in 1u64..50,
        ) {
            proptest::prop_assume!(range_start + range_len <= file_len);
            let range = LineRange { start: range_start, end: range_start + range_len };
            let old: String = (1..=file_len).map(|n| format!("line {n}\n")).collect();

            // Insert `inserted` distinct lines at the very top. Even for a
            // range starting at line 1 this touches no anchored line — the
            // insertion hunk ends where the range begins — so it must
            // shift, never outdate.
            let above: String = (0..inserted)
                .map(|n| format!("inserted {n}\n"))
                .chain((1..=file_len).map(|n| format!("line {n}\n")))
                .collect();
            proptest::prop_assert_eq!(
                map_range(old.as_bytes(), above.as_bytes(), range),
                Some(LineRange { start: range.start + inserted, end: range.end + inserted })
            );

            // Append strictly below the range: the range must not move.
            let below: String = (1..=file_len)
                .map(|n| format!("line {n}\n"))
                .chain((0..inserted).map(|n| format!("appended {n}\n")))
                .collect();
            proptest::prop_assert_eq!(
                map_range(old.as_bytes(), below.as_bytes(), range),
                Some(range)
            );
        }
    }

    // @relation(anchor.projection, scope=function, role=Verifies)
    #[test]
    fn map_range_handles_edges() {
        let old = b"a\nb\nc\nd\n".as_slice();
        // An insertion exactly at the range start shifts it; one exactly
        // at its end leaves it alone.
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

    /// A copy of `anchor` whose recorded commit is a made-up id that was
    /// never written to the repository — standing in for "gc'd away"
    /// without actually having to run gc in a unit test; `has_object`
    /// answers `false` either way.
    fn with_missing_commit(anchor: &Anchor) -> Anchor {
        let mut forged = anchor.clone();
        let fake = gix::ObjectId::from_hex(b"0123456789abcdef0123456789abcdef01234567").unwrap();
        forged.commit.copy_from_slice(fake.as_slice());
        forged
    }

    // @relation(anchor.fuzzy-fallback, scope=function, role=Verifies)
    #[test]
    fn project_exact_reports_the_anchor_commit_as_missing_and_project_degrades() {
        let dir = repo();
        std::fs::write(dir.path().join("file.txt"), numbered(1..=10)).unwrap();
        commit_all(dir.path(), "one");
        let git_repo = gix::open(dir.path()).unwrap();
        let anchor = capture(&git_repo, "HEAD", "file.txt", range(5, 6)).unwrap();

        let edited = format!("added a\nadded b\n{}", numbered(1..=10));
        std::fs::write(dir.path().join("file.txt"), edited).unwrap();
        commit_all(dir.path(), "two");
        let git_repo = gix::open(dir.path()).unwrap();

        let anchor = with_missing_commit(&anchor);
        assert!(matches!(
            project_exact(&git_repo, &anchor, "HEAD"),
            Err(Error::AnchorCommitMissing(_))
        ));
        // The umbrella entry point degrades to the context fallback
        // instead of failing (`anchor.fuzzy-fallback`), and recovers the
        // same relocation the exact path would have found.
        assert_eq!(
            project(&git_repo, &anchor, "HEAD").unwrap(),
            Projection::Relocated {
                path: "file.txt".to_owned(),
                lines: range(7, 8),
            }
        );
    }

    // @relation(anchor.fuzzy-fallback, scope=function, role=Verifies)
    #[test]
    fn project_from_context_relocates_across_an_edit_above_the_range() {
        let dir = repo();
        std::fs::write(dir.path().join("file.txt"), numbered(1..=10)).unwrap();
        commit_all(dir.path(), "one");
        let git_repo = gix::open(dir.path()).unwrap();
        let anchor = capture(&git_repo, "HEAD", "file.txt", range(5, 6)).unwrap();

        let edited = format!("added a\nadded b\n{}", numbered(1..=10));
        std::fs::write(dir.path().join("file.txt"), edited).unwrap();
        commit_all(dir.path(), "two");
        let git_repo = gix::open(dir.path()).unwrap();

        // Same answer `project_exact` would give, but derived with no
        // reference at all to the anchor's own (still very much present)
        // commit — exercising the exact code path that stands in once it
        // is gone.
        assert_eq!(
            project_from_context(&git_repo, &anchor, "HEAD").unwrap(),
            Projection::Relocated {
                path: "file.txt".to_owned(),
                lines: range(7, 8),
            }
        );
    }

    // @relation(anchor.fuzzy-fallback, scope=function, role=Verifies)
    #[test]
    fn project_from_context_reports_outdated_when_no_window_matches_well() {
        let dir = repo();
        std::fs::write(dir.path().join("file.txt"), numbered(1..=10)).unwrap();
        commit_all(dir.path(), "one");
        let git_repo = gix::open(dir.path()).unwrap();
        let anchor = capture(&git_repo, "HEAD", "file.txt", range(5, 6)).unwrap();

        // A wholesale rewrite leaves nothing resembling the captured
        // neighborhood anywhere in the file.
        std::fs::write(dir.path().join("file.txt"), "totally\nunrelated\ncontent\n").unwrap();
        commit_all(dir.path(), "two");
        let git_repo = gix::open(dir.path()).unwrap();

        assert_eq!(
            project_from_context(&git_repo, &anchor, "HEAD").unwrap(),
            Projection::Outdated {
                path: "file.txt".to_owned(),
            }
        );
    }

    // @relation(anchor.fuzzy-fallback, scope=function, role=Verifies)
    #[test]
    fn project_from_context_reports_deleted() {
        let dir = repo();
        std::fs::write(dir.path().join("file.txt"), numbered(1..=10)).unwrap();
        commit_all(dir.path(), "one");
        let git_repo = gix::open(dir.path()).unwrap();
        let anchor = capture(&git_repo, "HEAD", "file.txt", range(5, 6)).unwrap();

        std::fs::remove_file(dir.path().join("file.txt")).unwrap();
        std::fs::write(dir.path().join("unrelated.txt"), "different\n").unwrap();
        commit_all(dir.path(), "two");
        let git_repo = gix::open(dir.path()).unwrap();

        assert_eq!(
            project_from_context(&git_repo, &anchor, "HEAD").unwrap(),
            Projection::Deleted
        );
    }

    // @relation(anchor.fuzzy-fallback, scope=function, role=Verifies)
    #[test]
    fn project_from_context_of_a_whole_file_anchor_survives_any_edit() {
        let dir = repo();
        std::fs::write(dir.path().join("file.txt"), numbered(1..=10)).unwrap();
        commit_all(dir.path(), "one");
        let git_repo = gix::open(dir.path()).unwrap();
        let anchor = capture(&git_repo, "HEAD", "file.txt", None).unwrap();

        let edited = numbered(1..=10).replace("line 5\n", "line five\n");
        std::fs::write(dir.path().join("file.txt"), edited).unwrap();
        commit_all(dir.path(), "two");
        let git_repo = gix::open(dir.path()).unwrap();

        assert_eq!(
            project_from_context(&git_repo, &anchor, "HEAD").unwrap(),
            Projection::Relocated {
                path: "file.txt".to_owned(),
                lines: None,
            }
        );
    }
}
