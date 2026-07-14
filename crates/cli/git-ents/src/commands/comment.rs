//! `git ents comment`: a thin wrapper around `ents_forge::comment`'s
//! business logic — this module only resolves the signer/actor identity
//! against [`LocalRoot`], translates a reached `Outcome` into a CLI-facing
//! [`Result`] (`crate::mutate::outcome_to_result`), and renders the
//! machine-readable listing, exactly as every other mutation command does.
//! Every operation is the library call itself (`lens.parity`); nothing
//! here re-implements one.

use ents_forge::comment;
use ents_forge::comment::{Comment, ListFilter, Listed, NewComment};
use ents_receive::Identity;

use super::{actor, signer};
use crate::error::Result;
use crate::mutate::outcome_to_result;
use crate::root::LocalRoot;

/// `git ents comment list`: every comment recorded in this repository.
///
/// # Errors
///
/// Propagates a ref-store or object read failure.
pub fn list(root: &LocalRoot) -> Result<Vec<(String, Comment)>> {
    Ok(comment::list(&root.refs, &root.objects)?)
}

/// `git ents comment list [--worktree] [--state ...] [--context ...]`:
/// matching comments with each anchor projected onto the working tree
/// (with `worktree`) or `HEAD`, plus the refs whose stored tree this
/// build could not read back (reported after the listing, never
/// silently dropped).
///
/// # Errors
///
/// Propagates a ref-store, object read, or projection failure.
pub fn list_projected(
    root: &LocalRoot,
    worktree: bool,
    filter: &ListFilter,
) -> Result<(Vec<Listed>, Vec<ents_forge::Unreadable>)> {
    Ok(comment::list_projected(
        &root.refs,
        &root.objects,
        &root.path,
        worktree,
        filter,
    )?)
}

/// `git ents comment add`: create a comment about something.
///
/// # Errors
///
/// [`crate::error::Error::Forge`] if the comment is about nothing, its
/// arguments do not parse, or anchoring, serialization, or `receive`
/// itself fails; see [`crate::mutate::outcome_to_result`] for how a
/// reached refusal renders.
pub fn add(root: &LocalRoot, new: NewComment, key: Option<std::path::PathBuf>) -> Result<String> {
    let signer = signer(root, key)?;
    let identity = Identity {
        actor: actor(&signer),
        sign: &|payload| signer.sign(payload),
    };
    let (id, outcome) = comment::add(
        &root.refs,
        &root.objects,
        &root.events,
        &root.path,
        new,
        &identity,
        root.mode(),
    )?;
    outcome_to_result(outcome, None)?;
    Ok(id)
}

/// `git ents comment reply`: a comment whose parent is `parent_id`.
///
/// # Errors
///
/// See [`add`]; additionally [`ents_forge::Error::NotFound`] (wrapped)
/// when `parent_id` names no comment.
pub fn reply(
    root: &LocalRoot,
    parent_id: &str,
    body: String,
    key: Option<std::path::PathBuf>,
) -> Result<String> {
    let signer = signer(root, key)?;
    let identity = Identity {
        actor: actor(&signer),
        sign: &|payload| signer.sign(payload),
    };
    let (id, outcome) = comment::reply(
        &root.refs,
        &root.objects,
        &root.events,
        parent_id,
        body,
        &identity,
        root.mode(),
    )?;
    outcome_to_result(outcome, None)?;
    Ok(id)
}

/// `git ents comment resolve` / `reopen`: record the state mutation on the
/// comment's own ref.
///
/// # Errors
///
/// See [`add`].
pub fn set_state(
    root: &LocalRoot,
    id: &str,
    resolve: bool,
    key: Option<std::path::PathBuf>,
) -> Result<()> {
    let signer = signer(root, key)?;
    let identity = Identity {
        actor: actor(&signer),
        sign: &|payload| signer.sign(payload),
    };
    let outcome = if resolve {
        comment::resolve(
            &root.refs,
            &root.objects,
            &root.events,
            id,
            &identity,
            root.mode(),
            Some(&signer.public_openssh()),
        )?
    } else {
        comment::reopen(
            &root.refs,
            &root.objects,
            &root.events,
            id,
            &identity,
            root.mode(),
            Some(&signer.public_openssh()),
        )?
    };
    outcome_to_result(outcome, None)?;
    Ok(())
}

/// `git ents comment show`: `id`'s comment and, when anchored, its anchor
/// projected onto `rev` or the working tree.
///
/// # Errors
///
/// [`crate::error::Error::Forge`] (wrapping [`ents_forge::Error::NotFound`])
/// if `id` has no comment ref.
pub fn show(
    root: &LocalRoot,
    id: &str,
    rev: &str,
    worktree: bool,
) -> Result<(
    Comment,
    Option<(ents_anchor::Anchor, ents_anchor::Projection)>,
)> {
    Ok(comment::show(
        &root.refs,
        &root.objects,
        &root.path,
        id,
        rev,
        worktree,
    )?)
}

/// One record of `git ents comment list --porcelain`'s stable
/// machine-readable form (`lens.parity`: id, state, projected location,
/// and body, sufficient for an agent to enumerate and resolve every open
/// comment with no editor attached):
///
/// ```text
/// <id> <state> <projection> <location>
/// context <c>        (only when the comment names one)
/// parent <id>        (only when the comment is a reply)
/// \t<body line>      (every body line, tab-prefixed)
/// ```
///
/// `projection` is `current`, `relocated`, `outdated`, or `deleted`, and
/// `-` for a comment with no anchor; `location` is `path:start-end`
/// (`path` alone for a whole-file anchor) and `-` when there is no anchor
/// or the file is gone. Records are separated by one blank line — a blank
/// body line renders as a lone tab, so it can never terminate a record.
#[must_use]
pub fn porcelain(rows: &[Listed]) -> String {
    let mut out = String::new();
    for (index, row) in rows.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        let (projection, location) = match (&row.projection, &row.anchor) {
            (Some(projection), Some(anchor)) => porcelain_projection(projection, anchor),
            _ => ("-".to_owned(), "-".to_owned()),
        };
        out.push_str(&format!(
            "{} {} {} {}\n",
            row.id, row.comment.state, projection, location
        ));
        if let Some(context) = &row.comment.context {
            out.push_str(&format!("context {context}\n"));
        }
        if let Some(parent) = &row.comment.parent {
            out.push_str(&format!("parent {parent}\n"));
        }
        for line in row.comment.body.lines() {
            out.push('\t');
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// The `(projection, location)` columns of one porcelain record.
fn porcelain_projection(
    projection: &ents_anchor::Projection,
    anchor: &ents_anchor::Anchor,
) -> (String, String) {
    use ents_anchor::Projection;
    match projection {
        Projection::Current => ("current".to_owned(), location(&anchor.path, anchor.lines)),
        Projection::Relocated { path, lines } => ("relocated".to_owned(), location(path, *lines)),
        Projection::Outdated { path } => ("outdated".to_owned(), location(path, None)),
        Projection::Deleted => ("deleted".to_owned(), "-".to_owned()),
    }
}

fn location(path: &str, lines: Option<ents_anchor::LineRange>) -> String {
    match lines {
        Some(range) => format!("{path}:{}-{}", range.start, range.end),
        None => path.to_owned(),
    }
}
