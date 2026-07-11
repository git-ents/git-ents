//! `git ents comment`: anchor a comment to code and show it back,
//! projected onto a revision (`model.comment`, `anchor.definition`,
//! `anchor.projection`).

use ents_anchor::{Anchor, LineRange, Projection, project, snippet};
use ents_model::{Comment, namespace};
use facet_git_tree::RawTree;
use gix_ref_store::RefStoreRead;

use super::{actor, signer};
use crate::error::{Error, Result};
use crate::mutate::{Identity, outcome_to_result, propose_entity};
use crate::root::LocalRoot;

/// `git ents comment add`: anchor `body` to `path` (optionally `lines`) at
/// `rev`.
///
/// # Errors
///
/// [`Error::InvalidArgument`] if `lines` does not parse as `<start>[:<end>]`;
/// otherwise propagates capture, serialization, or `receive` failures.
pub fn add(
    root: &LocalRoot,
    path: &str,
    body: String,
    lines: Option<String>,
    rev: &str,
    key: Option<std::path::PathBuf>,
) -> Result<String> {
    let repo = gix::open(&root.path)?;
    let range = lines.map(|text| parse_line_range(&text)).transpose()?;
    let anchor = ents_anchor::capture(&repo, rev, path, range)?;

    let anchor_tree = facet_git_tree::serialize_into(&anchor, &root.objects)?;
    let comment = Comment {
        body,
        anchor: RawTree::new(anchor_tree),
    };

    // The comment's id is its own genesis tip's short oid, known only once
    // the commit is built — `propose_entity` builds it internally, so this
    // command derives the ref name from a locally generated id instead
    // (`meta-ref.granularity`: one ref per comment).
    let id = uuid::Uuid::new_v4().simple().to_string();
    let ref_name = namespace::comment_ref(&id)?;

    let signer = signer(root, key)?;
    let identity = Identity {
        actor: actor(&signer),
        signer: &signer,
    };
    let outcome = propose_entity(
        &root.refs,
        &root.objects,
        &root.events,
        ref_name,
        &comment,
        &identity,
        &format!("Comment on {path}"),
        root.mode(),
    )?;
    outcome_to_result(outcome, None)?;
    Ok(id)
}

/// `git ents comment show`: `id`'s anchor (projected onto `rev`), anchored
/// text, and body.
///
/// # Errors
///
/// [`Error::NotFound`] if `id` has no comment ref.
pub fn show(root: &LocalRoot, id: &str, rev: &str) -> Result<(Comment, Anchor, Projection)> {
    let ref_name = namespace::comment_ref(id)?;
    let Some(tip) = root.refs.get(ref_name.as_ref())? else {
        return Err(Error::NotFound {
            what: format!("comment {id}"),
        });
    };
    let tree = super::commit_tree(&root.objects, tip)?;
    let comment = facet_git_tree::deserialize::<Comment>(&tree, &root.objects)?;
    let anchor = facet_git_tree::deserialize::<Anchor>(&comment.anchor.oid(), &root.objects)?;

    let repo = gix::open(&root.path)?;
    let projection = project(&repo, &anchor, rev)?;
    let _ = snippet(&anchor)?; // Confirm the anchored text still reads back.
    Ok((comment, anchor, projection))
}

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
