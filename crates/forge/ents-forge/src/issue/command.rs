//! The `issue` command's business logic: create an issue
//! (`model.issue`), edit its state/assignees/labels, list, and read one
//! back.
//!
//! Generalized over the same trait-object/generic seam
//! `crate::comment::command` uses (`&dyn RefStore`/`RefStoreRead`,
//! `impl Find`/`Find + Write`, `&dyn ents_receive::EventSink`), so a
//! composition root wires the concrete types and calls these functions,
//! never the other way around (`lens.parity`). Obtaining a title/body from
//! an interactive editor is a frontend concern — the CLI's own
//! `commands::issue` resolves that before calling [`new`] here — not an
//! operation this crate's library layer offers, mirroring how
//! `crate::comment::command` never touches a terminal either.

use ents_model::MemberId;
use ents_receive::{Identity, Mode, Outcome, propose_entity, propose_genesis};
use gix_hash::ObjectId;
use gix_object::{CommitRef, Find, Kind};

use super::Issue;
use crate::error::{Error, Result};

/// The tree of the commit at `oid` — duplicated from
/// `crate::comment::command`'s own copy; see that copy's doc for why this
/// codebase accepts one small copy per module rather than a shared helper.
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

/// Read the [`Issue`] at `id`'s ref tip, or [`Error::NotFound`] when no
/// such ref exists.
fn issue_at(
    refs: &dyn gix_ref_store::RefStoreRead,
    objects: &impl Find,
    id: &str,
) -> Result<Issue> {
    let ref_name = ents_model::namespace::issue_ref(id)?;
    let Some(tip) = refs.get(ref_name.as_ref())? else {
        return Err(Error::NotFound {
            what: format!("issue {id}"),
        });
    };
    let tree = commit_tree(objects, tip)?;
    Ok(facet_git_tree::deserialize(&tree, objects)?)
}

/// What `git ents issue new` writes.
#[derive(Debug, Clone)]
pub struct NewIssue {
    /// The issue's title.
    pub title: String,
    /// The issue's body.
    pub body: String,
    /// The issue's initial state; a new issue's state has no platform
    /// default (`model.issue`: custom states are schema, not a platform
    /// feature) — the CLI's own default is `"open"`.
    pub state: String,
    /// Members assigned to the issue at creation.
    pub assignees: Vec<MemberId>,
    /// Labels attached at creation.
    pub labels: Vec<String>,
}

/// `git ents issue new`: create an issue at `refs/meta/issues/<id>`, where
/// `<id>` is the oid of the issue's own genesis commit — sign-then-name,
/// never a locally minted id (`model.issue`, `meta-ref.identity-binding`).
///
/// # Errors
///
/// Propagates serialization or `receive` failures.
// @relation(model.issue, meta-ref.identity-binding, lens.parity, scope=function)
pub fn new(
    refs: &dyn gix_ref_store::RefStore,
    objects: &(impl Find + gix_object::Write),
    events: &dyn ents_receive::EventSink,
    new: NewIssue,
    identity: &Identity<'_>,
    mode: Mode,
) -> Result<(String, Outcome)> {
    let issue = Issue {
        title: new.title,
        body: new.body,
        state: new.state,
        assignees: new.assignees,
        labels: new.labels,
    };
    let subject = format!("Open issue: {}", issue.title);
    let (ref_name, outcome) = propose_genesis(
        refs,
        objects,
        events,
        &issue,
        |oid| ents_model::namespace::issue_ref(&oid.to_string()),
        identity,
        &subject,
        mode,
    )?;
    Ok((crate::genesis_id(&ref_name), outcome))
}

/// What `git ents issue edit` changes; a field left `None` is left
/// untouched.
#[derive(Debug, Clone, Default)]
pub struct EditIssue {
    /// Replace the issue's state, or leave it unchanged.
    pub state: Option<String>,
    /// Replace the issue's assignees, or leave them unchanged.
    pub assignees: Option<Vec<MemberId>>,
    /// Replace the issue's labels, or leave them unchanged.
    pub labels: Option<Vec<String>>,
}

/// `git ents issue edit`: mutate `id`'s state, assignees, and/or labels as
/// an ordinary mutation commit on the issue's own ref, on top of its
/// current tip.
///
/// # Errors
///
/// [`Error::NotFound`] if `id` has no issue ref; otherwise propagates
/// serialization or `receive` failures.
// @relation(model.issue, lens.parity, scope=function)
pub fn edit(
    refs: &dyn gix_ref_store::RefStore,
    objects: &(impl Find + gix_object::Write),
    events: &dyn ents_receive::EventSink,
    id: &str,
    edit: EditIssue,
    identity: &Identity<'_>,
    mode: Mode,
) -> Result<Outcome> {
    let mut issue = issue_at(refs, objects, id)?;
    if let Some(state) = edit.state {
        issue.state = state;
    }
    if let Some(assignees) = edit.assignees {
        issue.assignees = assignees;
    }
    if let Some(labels) = edit.labels {
        issue.labels = labels;
    }
    let ref_name = ents_model::namespace::issue_ref(id)?;
    Ok(propose_entity(
        refs,
        objects,
        events,
        ref_name,
        &issue,
        identity,
        &format!("Edit issue {id}"),
        mode,
    )?)
}

/// `git ents issue list`: every issue recorded in this repository.
///
/// A ref whose tip this build cannot read back as an [`Issue`] is
/// silently absent here — a caller that must surface those refs instead
/// of dropping them (`ents-web`'s issues page) uses [`list_all`], which
/// this is the readable-rows-only view of.
///
/// # Errors
///
/// Propagates a ref-store or object read failure.
///
/// # Examples
///
/// ```
/// use ents_forge::issue::list;
/// use ents_testutil::{MemRefStore, ObjectStore};
///
/// let refs = MemRefStore::default();
/// let objects = ObjectStore::default();
/// assert!(list(&refs, &objects).expect("reads").is_empty());
/// ```
pub fn list(
    refs: &dyn gix_ref_store::RefStoreRead,
    objects: &impl Find,
) -> Result<Vec<(String, Issue)>> {
    Ok(list_all(refs, objects)?.0)
}

/// [`list`] plus the refs it could not read: every readable issue, and
/// one [`crate::Unreadable`] per `refs/meta/issues/*` ref whose tip this
/// build's [`Issue`] shape could not read back — the issue counterpart to
/// [`crate::comment::list_all`], with the same never-silently-dropped
/// contract (see [`crate::Unreadable`]'s own doc).
///
/// # Errors
///
/// Propagates a ref-store read failure — a per-ref *entity* read failure
/// is a row in the second vec, never an error.
///
/// # Examples
///
/// ```
/// use ents_forge::issue::list_all;
/// use ents_testutil::{MemRefStore, ObjectStore};
///
/// let refs = MemRefStore::default();
/// let objects = ObjectStore::default();
/// let (rows, unreadable) = list_all(&refs, &objects).expect("reads");
/// assert!(rows.is_empty());
/// assert!(unreadable.is_empty());
/// ```
pub fn list_all(
    refs: &dyn gix_ref_store::RefStoreRead,
    objects: &impl Find,
) -> Result<crate::Listing<Issue>> {
    let mut out = Vec::new();
    let mut unreadable = Vec::new();
    for entry in refs.iter_prefix("refs/meta/issues/")? {
        let (name, tip) = entry?;
        let path = name.as_bstr().to_string();
        let Some(id) = path.strip_prefix("refs/meta/issues/") else {
            continue;
        };
        match commit_tree(objects, tip)
            .and_then(|tree| Ok(facet_git_tree::deserialize::<Issue>(&tree, objects)?))
        {
            Ok(issue) => out.push((id.to_owned(), issue)),
            Err(error) => unreadable.push(crate::Unreadable {
                refname: path.clone(),
                error: error.to_string(),
            }),
        }
    }
    Ok((out, unreadable))
}

/// `git ents issue show`: `id`'s issue.
///
/// # Errors
///
/// [`Error::NotFound`] if `id` has no issue ref.
// @relation(model.issue, lens.parity, scope=function)
pub fn show(
    refs: &dyn gix_ref_store::RefStoreRead,
    objects: &impl Find,
    id: &str,
) -> Result<Issue> {
    issue_at(refs, objects, id)
}
