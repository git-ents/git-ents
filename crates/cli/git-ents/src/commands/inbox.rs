//! `git ents inbox`: list entities awaiting adoption and adopt them onto
//! their canonical ref (`sync.adoption-machinery`,
//! `sync.adoption-no-cherry-pick`).

use ents_model::namespace;
use ents_sync::resolve::{Heads, Merged, merge_heads};
use gix_ref_store::RefStoreRead;

use super::{actor, signer};
use crate::error::{Error, Result};
use crate::mutate::outcome_to_result;
use crate::root::LocalRoot;

/// `git ents inbox list`: every `refs/meta/inbox/<member>/<id>` entry.
///
/// # Errors
///
/// Propagates a ref-store read failure.
pub fn list(root: &LocalRoot) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for entry in root.refs.iter_prefix("refs/meta/inbox/")? {
        let (name, _) = entry?;
        let path = name.as_bstr().to_string();
        if let Some(rest) = path.strip_prefix("refs/meta/inbox/") {
            out.push(rest.to_owned());
        }
    }
    Ok(out)
}

/// `git ents inbox adopt`: fold `entry` (`<member>/<id>`) onto its
/// canonical ref (`refs/meta/<id>`) via [`merge_heads`], keeping the
/// author's original signed commit in ancestry
/// (`sync.adoption-no-cherry-pick`).
///
/// # Errors
///
/// [`Error::NotFound`] if `entry` has no inbox ref; [`Error::InvalidArgument`]
/// on a merge conflict (a human must resolve it before adoption can
/// complete — this phase does not implement interactive conflict
/// resolution); otherwise see [`crate::mutate::outcome_to_result`].
pub fn adopt(root: &LocalRoot, entry: &str, key: Option<std::path::PathBuf>) -> Result<()> {
    let Some((member, id)) = entry.split_once('/') else {
        return Err(Error::InvalidArgument(format!(
            "expected <member>/<id>, got {entry:?}"
        )));
    };
    let inbox_ref = namespace::inbox_ref(&ents_model::MemberId::new(member), id)?;
    let Some(theirs) = root.refs.get(inbox_ref.as_ref())? else {
        return Err(Error::NotFound {
            what: format!("inbox entry {entry}"),
        });
    };
    let canonical: gix::refs::FullName = format!("refs/meta/{id}")
        .try_into()
        .map_err(|_source| Error::InvalidArgument(format!("bad canonical ref for {id}")))?;
    let ours = root.refs.get(canonical.as_ref())?;

    let signer = signer(root, key)?;
    let author = actor(&signer);
    let heads = Heads {
        refname: canonical.clone(),
        ours,
        theirs,
    };
    let merged = merge_heads(
        &root.objects,
        &heads,
        &author,
        &format!("Adopt {entry}"),
        |payload| signer.sign(payload),
    )?;
    let tip = match merged {
        Merged::Tip(tip) => tip,
        Merged::Conflict(paths) => {
            let rendered = paths
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(Error::InvalidArgument(format!(
                "adoption conflict at: {rendered}"
            )));
        }
    };

    let proposal = ents_receive::Proposal {
        transitions: vec![ents_receive::RefTransition {
            name: canonical,
            old: ours,
            new: Some(tip),
        }],
        objects: vec![tip],
        auth: None,
    };
    let outcome = ents_receive::receive(
        &root.refs,
        &root.objects,
        &root.events,
        &proposal,
        root.mode(),
    )?;
    outcome_to_result(outcome, ours)?;
    Ok(())
}
