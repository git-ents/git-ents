//! `git ents issue`: a thin wrapper around `ents_forge::issue`'s business
//! logic, plus the one CLI-only piece that operation needs: composing a
//! title and body in `$GIT_EDITOR`/`$EDITOR` when `--title` is omitted
//! (mirroring `git commit`'s own editor fallback) — a frontend concern,
//! not an operation `ents_forge::issue` offers (`lens.parity`).

use std::io::Write as _;
use std::path::PathBuf;
use std::process::Command;

use ents_forge::Issue;
use ents_forge::issue::{self, EditIssue, NewIssue};
use ents_model::MemberId;
use ents_receive::Identity;

use super::{actor, signer};
use crate::error::{Error, Result};
use crate::mutate::outcome_to_result;
use crate::root::LocalRoot;

/// `git ents issue list`: every issue recorded in this repository.
///
/// # Errors
///
/// Propagates a ref-store or object read failure.
pub fn list(root: &LocalRoot) -> Result<Vec<(String, Issue)>> {
    Ok(issue::list(&root.refs, &root.objects)?)
}

/// `git ents issue show`: `id`'s issue.
///
/// # Errors
///
/// [`crate::error::Error::Forge`] (wrapping [`ents_forge::Error::NotFound`])
/// if `id` has no issue ref.
pub fn show(root: &LocalRoot, id: &str) -> Result<Issue> {
    Ok(issue::show(&root.refs, &root.objects, id)?)
}

/// `git ents issue new`: create an issue. When `title` is `None`, composes
/// the title and body interactively (see `compose_in_editor`).
///
/// # Errors
///
/// [`Error::InvalidArgument`] if no title was given and the interactively
/// composed message is empty (the editor path aborts, mirroring `git
/// commit`'s own empty-message abort); [`Error::Io`] if the editor cannot
/// be spawned or the scratch file cannot be read or written; otherwise see
/// [`crate::mutate::outcome_to_result`].
pub fn new(
    root: &LocalRoot,
    title: Option<String>,
    body: Option<String>,
    state: String,
    labels: Vec<String>,
    assignees: Vec<String>,
    key: Option<PathBuf>,
) -> Result<String> {
    let (title, body) = match title {
        Some(title) => (title, body.unwrap_or_default()),
        None => compose_in_editor()?
            .ok_or_else(|| Error::InvalidArgument("empty issue message, aborting".into()))?,
    };
    let signer = signer(root, key)?;
    let identity = Identity {
        actor: actor(&signer),
        sign: &|payload| signer.sign(payload),
    };
    let new = NewIssue {
        title,
        body,
        state,
        assignees: assignees.into_iter().map(MemberId::new).collect(),
        labels,
    };
    let (id, outcome) = issue::new(
        &root.refs,
        &root.objects,
        &root.events,
        new,
        &identity,
        root.mode(),
    )?;
    outcome_to_result(outcome, None)?;
    Ok(id)
}

/// `git ents issue edit`: mutate `id`'s state, assignees, and/or labels.
/// Assignees/labels replace the previous set entirely when at least one
/// value is given; an empty list leaves that field unchanged.
///
/// # Errors
///
/// See [`crate::mutate::outcome_to_result`].
pub fn edit(
    root: &LocalRoot,
    id: &str,
    state: Option<String>,
    labels: Vec<String>,
    assignees: Vec<String>,
    key: Option<PathBuf>,
) -> Result<()> {
    let signer = signer(root, key)?;
    let identity = Identity {
        actor: actor(&signer),
        sign: &|payload| signer.sign(payload),
    };
    let edit = EditIssue {
        state,
        labels: (!labels.is_empty()).then_some(labels),
        assignees: (!assignees.is_empty())
            .then(|| assignees.into_iter().map(MemberId::new).collect()),
    };
    let outcome = issue::edit(
        &root.refs,
        &root.objects,
        &root.events,
        id,
        edit,
        &identity,
        root.mode(),
    )?;
    outcome_to_result(outcome, None)?;
    Ok(())
}

/// Compose a title (first line) and body (remaining lines) by opening
/// `$GIT_EDITOR` (or `$EDITOR`, or `vi`) on a scratch file seeded with a
/// `#`-prefixed instructions footer; lines starting with `#` are stripped
/// on read-back. Returns `None` (mirroring `git commit`'s own
/// empty-message abort) when the title line is empty after stripping.
///
/// # Errors
///
/// [`Error::Io`] if the scratch file cannot be created, written, or read,
/// or the editor process cannot be spawned or exits with a failure status.
fn compose_in_editor() -> Result<Option<(String, String)>> {
    let editor = std::env::var("GIT_EDITOR")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_owned());

    let mut file = tempfile::NamedTempFile::new().map_err(|source| Error::Io {
        path: std::env::temp_dir(),
        source,
    })?;
    let path = file.path().to_owned();
    writeln!(
        file,
        "\n# First line is the title, the rest is the body.\n\
         # Lines starting with '#' are stripped; an empty title aborts."
    )
    .map_err(|source| Error::Io {
        path: path.clone(),
        source,
    })?;
    file.flush().map_err(|source| Error::Io {
        path: path.clone(),
        source,
    })?;

    let status = Command::new(&editor)
        .arg(&path)
        .status()
        .map_err(|source| Error::Io {
            path: path.clone(),
            source,
        })?;
    if !status.success() {
        return Err(Error::Io {
            path: path.clone(),
            source: std::io::Error::other(format!("{editor} exited with {status}")),
        });
    }

    let contents = std::fs::read_to_string(&path).map_err(|source| Error::Io {
        path: path.clone(),
        source,
    })?;
    let mut lines = contents.lines().filter(|line| !line.starts_with('#'));
    let title = lines.next().unwrap_or("").trim();
    if title.is_empty() {
        return Ok(None);
    }
    let body = lines.collect::<Vec<_>>().join("\n");
    Ok(Some((title.to_owned(), body.trim_end().to_owned())))
}
