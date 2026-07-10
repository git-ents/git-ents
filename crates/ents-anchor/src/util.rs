//! Small `gix` plumbing helpers shared by [`crate::capture`] and
//! [`crate::project`]/[`crate::project_exact`]/[`crate::project_from_context`].
//!
//! Nothing here is public API; each function is a thin, single-purpose
//! wrapper over a `gix::Repository` lookup, kept out of the call sites that
//! use it so the projection and capture logic reads as policy rather than
//! plumbing.

use gix::ObjectId;
use gix::bstr::ByteSlice as _;

use crate::error::{Error, Result};

/// Resolve `revision` (a hex id, ref name, or revspec) to the commit it
/// names in `repo`.
pub(crate) fn resolve_commit<'repo>(
    repo: &'repo gix::Repository,
    revision: &str,
) -> Result<gix::Commit<'repo>> {
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

/// Look up the commit `id` names directly, with no revision parsing — for an
/// [`crate::Anchor`]'s own recorded commit, which already names a concrete
/// object rather than an arbitrary revision.
pub(crate) fn commit_at(repo: &gix::Repository, id: ObjectId) -> Result<gix::Commit<'_>> {
    let resolve = || Error::Resolve(id.to_string());
    repo.find_object(id)
        .map_err(|_error| resolve())?
        .peel_to_kind(gix::object::Kind::Commit)
        .map_err(|_error| resolve())?
        .try_into_commit()
        .map_err(|_error| resolve())
}

/// Read the full contents of the blob at `id`.
pub(crate) fn read_blob(repo: &gix::Repository, id: ObjectId) -> Result<Vec<u8>> {
    Ok(repo
        .find_blob(id)
        .map_err(|error| Error::Object(error.to_string()))?
        .take_data())
}

/// The text of the 1-based inclusive `range` within `data`, or
/// [`Error::LinesOutOfRange`] (naming `path`) when the range does not fit.
pub(crate) fn lines_of(data: &[u8], path: &str, range: crate::LineRange) -> Result<String> {
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
