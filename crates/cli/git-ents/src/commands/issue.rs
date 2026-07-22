//! `git ents issue`: a thin wrapper around `ents_forge::issue`'s business
//! logic — the `$GIT_EDITOR` fallback for an omitted `--title` lives in
//! [`crate::compose`], driven by the `ents::compose` attributes on
//! [`ents_forge::issue::IssueAction`], not here (`lens.parity`).

use std::path::PathBuf;

use ents_forge::Issue;
use ents_forge::issue::{self, EditIssue, NewIssue};
use ents_model::MemberId;
use ents_receive::Identity;

use super::{actor, signer};
use crate::error::Result;
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

/// `git ents issue new`: create an issue.
///
/// # Errors
///
/// See [`crate::mutate::outcome_to_result`].
pub fn new(
    root: &LocalRoot,
    title: String,
    body: String,
    state: String,
    labels: Vec<String>,
    assignees: Vec<String>,
    key: Option<PathBuf>,
) -> Result<String> {
    let signer = signer(root, key)?;
    let identity = Identity {
        actor: actor(&signer),
        author: None,
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
        author: None,
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
