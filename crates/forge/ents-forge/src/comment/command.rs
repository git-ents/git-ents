//! The `comment` command's business logic: anchor a comment to code and
//! show it back, projected onto a revision (`model.comment`,
//! `anchor.definition`, `anchor.projection`).
//!
//! Generalized over the same trait-object/generic seam
//! `ents_effect::run` uses (`&dyn RefStore`/`RefStoreRead`,
//! `impl Find`/`Find + Write`, `&dyn ents_receive::EventSink`) rather than
//! any concrete composition-root type, so this crate never depends on a
//! CLI or a specific store implementation — a composition root wires the
//! concrete types and calls these functions, never the other way around.

use ents_anchor::{Anchor, LineRange, Projection, project, snippet};
use ents_receive::{Identity, Mode, Outcome, propose_entity};
use facet_git_tree::RawTree;
use gix_hash::ObjectId;
use gix_object::{CommitRef, Find, Kind, Write};
use gix_ref_store::{RefStore, RefStoreRead};

use super::Comment;
use crate::error::{Error, Result};

/// The tree of the commit at `oid` — read back into a typed entity by
/// every command below. Duplicated in `ents_effect::run` and the CLI's own
/// `crate::commands::commit_tree` rather than shared: three ~15-line
/// copies of "read a commit's tree oid via `Find`" is the accepted pattern
/// in this codebase, not a gap to close with a shared utility.
fn commit_tree(objects: &impl Find, oid: ObjectId) -> Result<ObjectId> {
    let mut buf = Vec::new();
    let data = objects
        .try_find(&oid, &mut buf)
        .map_err(|source| Error::InvalidArgument(source.to_string()))?
        .ok_or_else(|| Error::NotFound {
            what: oid.to_string(),
        })?;
    if data.kind != Kind::Commit {
        return Err(Error::NotFound {
            what: oid.to_string(),
        });
    }
    let commit = CommitRef::from_bytes(data.data, oid.kind())
        .map_err(|source| Error::InvalidArgument(source.to_string()))?;
    Ok(commit.tree())
}

/// `git ents comment list`: every comment recorded in this repository.
///
/// # Errors
///
/// Propagates a ref-store or object read failure.
///
/// # Examples
///
/// ```
/// use ents_forge::comment::list;
/// use ents_testutil::{ObjectStore, MemRefStore};
///
/// let refs = MemRefStore::default();
/// let objects = ObjectStore::default();
/// assert!(list(&refs, &objects).expect("reads").is_empty());
/// ```
pub fn list(refs: &dyn RefStoreRead, objects: &impl Find) -> Result<Vec<(String, Comment)>> {
    let mut out = Vec::new();
    for entry in refs.iter_prefix("refs/meta/comments/")? {
        let (name, tip) = entry?;
        let path = name.as_bstr().to_string();
        let Some(id) = path.strip_prefix("refs/meta/comments/") else {
            continue;
        };
        let tree = commit_tree(objects, tip)?;
        if let Ok(comment) = facet_git_tree::deserialize::<Comment>(&tree, objects) {
            out.push((id.to_owned(), comment));
        }
    }
    Ok(out)
}

/// `git ents comment add`: anchor `body` to `path` (optionally `lines`) at
/// `rev`.
///
/// Returns the generated comment id alongside the raw
/// [`Outcome`] `receive` reached — callers interpret it themselves (the
/// CLI's own `outcome_to_result`, for instance), the same shape
/// `ents_effect::run::run_one` returns its own raw `Outcome` in.
///
/// # Errors
///
/// [`Error::InvalidArgument`] if `lines` does not parse as `<start>[:<end>]`;
/// otherwise propagates capture, serialization, or `receive` failures.
#[expect(
    clippy::too_many_arguments,
    reason = "one input per capture/build/propose step, mirrors ents_effect::run::run_one's shape"
)]
pub fn add(
    refs: &dyn RefStore,
    objects: &(impl Find + Write),
    events: &dyn ents_receive::EventSink,
    repo_path: &std::path::Path,
    path: &str,
    body: String,
    lines: Option<String>,
    rev: &str,
    identity: &Identity<'_>,
    mode: Mode,
) -> Result<(String, Outcome)> {
    let repo = gix::open(repo_path)?;
    let range = lines.map(|text| parse_line_range(&text)).transpose()?;
    let anchor = ents_anchor::capture(&repo, rev, path, range)?;

    let anchor_tree = facet_git_tree::serialize_into(&anchor, objects)?;
    let comment = Comment {
        body,
        anchor: RawTree::new(anchor_tree),
    };

    // The comment's id is its own genesis tip's short oid, known only once
    // the commit is built — `propose_entity` builds it internally, so this
    // command derives the ref name from a locally generated id instead
    // (`meta-ref.granularity`: one ref per comment).
    let id = uuid::Uuid::new_v4().simple().to_string();
    let ref_name = ents_model::namespace::comment_ref(&id)?;

    let outcome = propose_entity(
        refs,
        objects,
        events,
        ref_name,
        &comment,
        identity,
        &format!("Comment on {path}"),
        mode,
    )?;
    Ok((id, outcome))
}

/// `git ents comment show`: `id`'s anchor (projected onto `rev`), anchored
/// text, and body.
///
/// # Errors
///
/// [`Error::NotFound`] if `id` has no comment ref.
pub fn show(
    refs: &dyn RefStoreRead,
    objects: &impl Find,
    repo_path: &std::path::Path,
    id: &str,
    rev: &str,
) -> Result<(Comment, Anchor, Projection)> {
    let ref_name = ents_model::namespace::comment_ref(id)?;
    let Some(tip) = refs.get(ref_name.as_ref())? else {
        return Err(Error::NotFound {
            what: format!("comment {id}"),
        });
    };
    let tree = commit_tree(objects, tip)?;
    let comment = facet_git_tree::deserialize::<Comment>(&tree, objects)?;
    let anchor = facet_git_tree::deserialize::<Anchor>(&comment.anchor.oid(), objects)?;

    let repo = gix::open(repo_path)?;
    let projection = project(&repo, &anchor, rev)?;
    let _ = snippet(&anchor)?; // Confirm the anchored text still reads back.
    Ok((comment, anchor, projection))
}

/// Parse a `<start>[:<end>]` line-range argument.
///
/// # Errors
///
/// [`Error::InvalidArgument`] if either half does not parse as a `u64`.
fn parse_line_range(text: &str) -> Result<LineRange> {
    let (start, end) = match text.split_once(':') {
        Some((s, e)) => (s, e),
        None => (text, text),
    };
    let start: u64 = start
        .parse()
        .map_err(|_source| Error::InvalidArgument(format!("bad line range: {text}")))?;
    let end: u64 = end
        .parse()
        .map_err(|_source| Error::InvalidArgument(format!("bad line range: {text}")))?;
    Ok(LineRange { start, end })
}
