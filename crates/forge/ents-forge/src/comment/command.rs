//! The `comment` command's business logic: create a comment about
//! something (`model.comment`), reply to one (`model.comment-thread`),
//! resolve and reopen one (`model.comment-state`), and read them back —
//! projected onto a revision or the working tree (`anchor.projection`,
//! `anchor.working-tree`).
//!
//! Generalized over the same trait-object/generic seam
//! `ents_effect::run` uses (`&dyn RefStore`/`RefStoreRead`,
//! `impl Find`/`Find + Write`, `&dyn ents_receive::EventSink`) rather than
//! any concrete composition-root type, so this crate never depends on a
//! CLI or a specific store implementation — a composition root wires the
//! concrete types and calls these functions, never the other way around.
//! `lens.parity` makes this binding: the CLI, the web UI, and the editor
//! lens are three callers of exactly these functions.

use ents_anchor::{Anchor, LineRange, Projection, project, project_worktree, snippet};
use ents_receive::{Identity, Mode, Outcome, propose_entity};
use facet_git_tree::RawTree;
use gix_hash::ObjectId;
use gix_object::{CommitRef, Find, Kind, Write};
use gix_ref_store::{RefStore, RefStoreRead};

use super::Comment;
use super::entity::read_comment;
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

/// Read the [`Comment`] at `id`'s ref tip, through the legacy-shape
/// fallback (`meta-ref.migration`), or [`Error::NotFound`] when no such
/// ref exists.
fn comment_at(refs: &dyn RefStoreRead, objects: &impl Find, id: &str) -> Result<Comment> {
    let ref_name = ents_model::namespace::comment_ref(id)?;
    let Some(tip) = refs.get(ref_name.as_ref())? else {
        return Err(Error::NotFound {
            what: format!("comment {id}"),
        });
    };
    let tree = commit_tree(objects, tip)?;
    read_comment(&tree, objects)
}

/// `git ents comment list`: every comment recorded in this repository,
/// pre-migration trees included (`meta-ref.migration`).
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
        if let Ok(comment) = read_comment(&tree, objects) {
            out.push((id.to_owned(), comment));
        }
    }
    Ok(out)
}

/// One row of [`list_projected`]: the comment, and — when it carries an
/// anchor — that anchor and its projection onto the requested target.
#[derive(Debug, Clone)]
pub struct Listed {
    /// The comment's id (its refname below `refs/meta/comments/`).
    pub id: String,
    /// The comment itself.
    pub comment: Comment,
    /// The comment's anchor, read back from its embedded tree, or `None`
    /// for a comment with no anchor — carried alongside [`Listed::projection`]
    /// so a [`Projection::Current`] outcome still knows the anchored
    /// path and lines it applies at.
    pub anchor: Option<Anchor>,
    /// Where the anchor lands on the projection target, or `None` for a
    /// comment with no anchor.
    pub projection: Option<Projection>,
}

/// Which filters [`list_projected`] applies before projecting.
#[derive(Debug, Clone, Default)]
pub struct ListFilter {
    /// Keep only comments in this state (`model.comment-state`).
    pub state: Option<String>,
    /// Keep only comments naming this context (`model.comment-context`).
    pub context: Option<String>,
}

/// `git ents comment list [--worktree] [--state ...] [--context ...]`:
/// every matching comment, each anchor projected onto the working tree
/// (`anchor.working-tree`) when `worktree` is set, onto `HEAD` otherwise —
/// the listing `lens.parity` requires to be one library call shared by the
/// CLI's machine-readable form, the web UI, and the editor lens.
///
/// # Errors
///
/// Propagates a ref-store, object read, repository open, or projection
/// failure.
// @relation(lens.parity, model.comment-state, model.comment-context, scope=function)
pub fn list_projected(
    refs: &dyn RefStoreRead,
    objects: &impl Find,
    repo_path: &std::path::Path,
    worktree: bool,
    filter: &ListFilter,
) -> Result<Vec<Listed>> {
    let repo = gix::open(repo_path)?;
    let mut out = Vec::new();
    for (id, comment) in list(refs, objects)? {
        if let Some(state) = &filter.state
            && comment.state != *state
        {
            continue;
        }
        if let Some(context) = &filter.context
            && comment.context.as_ref() != Some(context)
        {
            continue;
        }
        let (anchor, projection) = match &comment.anchor {
            None => (None, None),
            Some(raw) => {
                let anchor = facet_git_tree::deserialize::<Anchor>(&raw.oid(), objects)?;
                let projection = if worktree {
                    project_worktree(&repo, &anchor, None)?
                } else {
                    project(&repo, &anchor, "HEAD")?
                };
                (Some(anchor), Some(projection))
            }
        };
        out.push(Listed {
            id,
            comment,
            anchor,
            projection,
        });
    }
    Ok(out)
}

/// What `git ents comment add` writes, before the mechanism-side
/// arguments: the body, what the comment is about (`model.comment` — at
/// least one of an anchored path, a context, or a parent), and where its
/// anchor captures from (`rev`, or the working tree per
/// `anchor.working-tree` when `worktree` is set).
#[derive(Debug, Clone)]
pub struct NewComment {
    /// The comment's body text.
    pub body: String,
    /// Repository-relative path to anchor to, or `None` for an unanchored
    /// comment (about a context or a parent instead).
    pub path: Option<String>,
    /// Lines to anchor, as `<start>[:<end>]`; requires `path`.
    pub lines: Option<String>,
    /// Revision to anchor against; ignored when `worktree` is set.
    pub rev: String,
    /// Anchor against the working tree's on-disk bytes instead of `rev`
    /// (`anchor.working-tree`).
    pub worktree: bool,
    /// Canonical ref path below `refs/meta/` of the entity this comment
    /// belongs to, such as `issues/<id>` (`model.comment-context`).
    pub context: Option<String>,
    /// Id of the comment this one replies to (`model.comment-thread`);
    /// [`reply`] is the porcelain shortcut that sets only this.
    pub parent: Option<String>,
}

/// `git ents comment add`: create a comment about something.
///
/// Returns the generated comment id alongside the raw
/// [`Outcome`] `receive` reached — callers interpret it themselves (the
/// CLI's own `outcome_to_result`, for instance), the same shape
/// `ents_effect::run::run_one` returns its own raw `Outcome` in.
///
/// # Errors
///
/// [`Error::InvalidArgument`] if the comment is about nothing — no path,
/// no context, no parent (`model.comment`: refused at creation by the
/// writing tool, never by the gate) — if `lines` does not parse as
/// `<start>[:<end>]` or names lines without a path, or if `context` does
/// not form a valid ref path below `refs/meta/`; [`Error::NotFound`] if
/// `parent` names no existing comment (`model.comment-thread`); otherwise
/// propagates capture, serialization, or `receive` failures.
// @relation(model.comment, model.comment-state, model.comment-context, model.comment-thread, lens.parity, scope=function)
pub fn add(
    refs: &dyn RefStore,
    objects: &(impl Find + Write),
    events: &dyn ents_receive::EventSink,
    repo_path: &std::path::Path,
    new: NewComment,
    identity: &Identity<'_>,
    mode: Mode,
) -> Result<(String, Outcome)> {
    // A comment about nothing is refused here, at creation, by the
    // writing tool — the gate stays content-agnostic (`model.comment`).
    if new.path.is_none() && new.context.is_none() && new.parent.is_none() {
        return Err(Error::InvalidArgument(
            "a comment must be about something: anchor it to a path, name a context, \
             or reply to a parent"
                .into(),
        ));
    }
    if new.lines.is_some() && new.path.is_none() {
        return Err(Error::InvalidArgument(
            "--lines needs a path to anchor to".into(),
        ));
    }
    if let Some(context) = &new.context {
        validate_context(context)?;
    }
    if let Some(parent) = &new.parent {
        // The parent must exist when the reply is created
        // (`model.comment-thread`).
        comment_at(refs, objects, parent)?;
    }

    let anchor = match &new.path {
        None => None,
        Some(path) => {
            let repo = gix::open(repo_path)?;
            let range = new.lines.map(|text| parse_line_range(&text)).transpose()?;
            Some(if new.worktree {
                ents_anchor::capture_worktree(&repo, path, range)?
            } else {
                ents_anchor::capture(&repo, &new.rev, path, range)?
            })
        }
    };
    let anchor = anchor
        .map(|anchor| facet_git_tree::serialize_into(&anchor, objects))
        .transpose()?
        .map(RawTree::new);

    let comment = Comment {
        body: new.body,
        // A new comment's state is `open` (`model.comment-state`).
        state: "open".to_owned(),
        anchor,
        context: new.context,
        parent: new.parent,
    };

    // The comment's id is its own genesis tip's short oid, known only once
    // the commit is built — `propose_entity` builds it internally, so this
    // command derives the ref name from a locally generated id instead
    // (`meta-ref.granularity`: one ref per comment).
    let id = uuid::Uuid::new_v4().simple().to_string();
    let ref_name = ents_model::namespace::comment_ref(&id)?;
    let subject = match &new.path {
        Some(path) => format!("Comment on {path}"),
        None => "Comment".to_owned(),
    };

    let outcome = propose_entity(
        refs, objects, events, ref_name, &comment, identity, &subject, mode,
    )?;
    Ok((id, outcome))
}

/// `git ents comment reply`: a comment whose parent is `parent_id`
/// (`model.comment-thread`) — its aboutness is inherited from its thread
/// root, so no anchor or context is required or set.
///
/// # Errors
///
/// [`Error::NotFound`] if `parent_id` names no existing comment; otherwise
/// see [`add`].
// @relation(model.comment-thread, lens.parity, scope=function)
pub fn reply(
    refs: &dyn RefStore,
    objects: &(impl Find + Write),
    events: &dyn ents_receive::EventSink,
    parent_id: &str,
    body: String,
    identity: &Identity<'_>,
    mode: Mode,
) -> Result<(String, Outcome)> {
    // The parent must exist when the reply is created.
    comment_at(refs, objects, parent_id)?;
    let comment = Comment {
        body,
        state: "open".to_owned(),
        anchor: None,
        context: None,
        parent: Some(parent_id.to_owned()),
    };
    let id = uuid::Uuid::new_v4().simple().to_string();
    let ref_name = ents_model::namespace::comment_ref(&id)?;
    let outcome = propose_entity(
        refs,
        objects,
        events,
        ref_name,
        &comment,
        identity,
        &format!("Reply to comment {parent_id}"),
        mode,
    )?;
    Ok((id, outcome))
}

/// `git ents comment resolve`: record state `resolved` as an ordinary
/// mutation commit on the comment's own ref — never a deletion, so the
/// conversation stays auditable (`model.comment-state`).
///
/// # Errors
///
/// [`Error::NotFound`] if `id` has no comment ref; otherwise propagates
/// read, serialization, or `receive` failures.
// @relation(model.comment-state, lens.parity, scope=function)
pub fn resolve(
    refs: &dyn RefStore,
    objects: &(impl Find + Write),
    events: &dyn ents_receive::EventSink,
    id: &str,
    identity: &Identity<'_>,
    mode: Mode,
) -> Result<Outcome> {
    set_state(refs, objects, events, id, "resolved", identity, mode)
}

/// `git ents comment reopen`: record state `open` again, the same way
/// [`resolve`] records `resolved` (`model.comment-state`).
///
/// # Errors
///
/// See [`resolve`].
// @relation(model.comment-state, lens.parity, scope=function)
pub fn reopen(
    refs: &dyn RefStore,
    objects: &(impl Find + Write),
    events: &dyn ents_receive::EventSink,
    id: &str,
    identity: &Identity<'_>,
    mode: Mode,
) -> Result<Outcome> {
    set_state(refs, objects, events, id, "open", identity, mode)
}

/// The shared state mutation [`resolve`] and [`reopen`] are: read the
/// comment at `id` — through the legacy fallback, so a pre-migration ref
/// is rewritten under the broadened struct by this very commit
/// (`meta-ref.migration`) — set `state`, and propose the new tree on top
/// of the old tip.
fn set_state(
    refs: &dyn RefStore,
    objects: &(impl Find + Write),
    events: &dyn ents_receive::EventSink,
    id: &str,
    state: &str,
    identity: &Identity<'_>,
    mode: Mode,
) -> Result<Outcome> {
    let mut comment = comment_at(refs, objects, id)?;
    comment.state = state.to_owned();
    let ref_name = ents_model::namespace::comment_ref(id)?;
    Ok(propose_entity(
        refs,
        objects,
        events,
        ref_name,
        &comment,
        identity,
        &format!("Mark comment {id} {state}"),
        mode,
    )?)
}

/// `git ents comment show`: `id`'s comment and — when it carries an
/// anchor — that anchor, projected onto `rev` or (with `worktree`) onto
/// the working tree (`anchor.working-tree`).
///
/// # Errors
///
/// [`Error::NotFound`] if `id` has no comment ref.
// @relation(lens.parity, scope=function)
pub fn show(
    refs: &dyn RefStoreRead,
    objects: &impl Find,
    repo_path: &std::path::Path,
    id: &str,
    rev: &str,
    worktree: bool,
) -> Result<(Comment, Option<(Anchor, Projection)>)> {
    let comment = comment_at(refs, objects, id)?;
    let Some(raw) = &comment.anchor else {
        return Ok((comment, None));
    };
    let anchor = facet_git_tree::deserialize::<Anchor>(&raw.oid(), objects)?;
    let repo = gix::open(repo_path)?;
    let projection = if worktree {
        project_worktree(&repo, &anchor, None)?
    } else {
        project(&repo, &anchor, rev)?
    };
    let _ = snippet(&anchor)?; // Confirm the anchored text still reads back.
    Ok((comment, Some((anchor, projection))))
}

/// The thread of `context` (`model.comment-context`,
/// `model.comment-thread`): every comment naming `context` directly, plus
/// every reply whose parent chain reaches one — an aggregation query over
/// decomposed comment refs, never a list any entity stores
/// (`meta-ref.granularity`). Rows come back sorted by id, roots and
/// replies alike; the `parent` field reconstructs the tree.
///
/// # Errors
///
/// Propagates a ref-store or object read failure.
// @relation(model.comment-context, model.comment-thread, scope=function)
pub fn thread(
    refs: &dyn RefStoreRead,
    objects: &impl Find,
    context: &str,
) -> Result<Vec<(String, Comment)>> {
    let all = list(refs, objects)?;
    let mut included: std::collections::BTreeMap<&str, &Comment> = all
        .iter()
        .filter(|(_, comment)| comment.context.as_deref() == Some(context))
        .map(|(id, comment)| (id.as_str(), comment))
        .collect();
    // Close over parent links: a reply names a comment already in the
    // thread, transitively — no comment stores a list of its replies.
    loop {
        let mut grew = false;
        for (id, comment) in &all {
            if included.contains_key(id.as_str()) {
                continue;
            }
            if let Some(parent) = &comment.parent
                && included.contains_key(parent.as_str())
            {
                included.insert(id.as_str(), comment);
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    Ok(included
        .into_iter()
        .map(|(id, comment)| (id.to_owned(), comment.clone()))
        .collect())
}

/// Validate a `model.comment-context` value: the canonical ref path below
/// `refs/meta/` of the entity the comment belongs to, such as
/// `issues/<id>` — checked by building the full refname it names.
fn validate_context(context: &str) -> Result<()> {
    let full = format!("refs/meta/{context}");
    if context.is_empty() || gix::refs::FullName::try_from(full).is_err() {
        return Err(Error::InvalidArgument(format!(
            "context {context:?} is not a ref path below refs/meta/"
        )));
    }
    Ok(())
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
