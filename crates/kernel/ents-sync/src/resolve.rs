//! One merge machinery for every same-tip reconciliation sync performs:
//! same-actor divergence (`sync.divergence-merge`,
//! `gate.same-actor-divergence`) and adoption — a maintainer folding an
//! inbox entity onto its canonical ref, or a member's self-run results onto
//! the canonical results ref (`sync.adoption-machinery`,
//! `gate.adoption-merge`).
//!
//! All three are the same operation: take the authorized side's current tip
//! (`ours`) and the head being folded in (`theirs`), merge their typed trees
//! three-way against the merge base ([`crate::merge::three_way`]), and record
//! the result as a two-parent merge commit signed by the placing member.
//! There is deliberately no separate adoption code path
//! (`sync.adoption-machinery`), and deliberately no cherry-pick: `theirs`
//! stays a parent, so the contributor's original signed commit — and its
//! attribution — remains in ancestry (`sync.adoption-no-cherry-pick`). A
//! cherry-pick would instead create a fresh commit by the placer and destroy
//! the author's signature, which no gate could detect after the fact, so this
//! property is the machinery's to keep, not the gate's.

use std::collections::HashSet;

use gix::bstr::BString;
use gix::refs::FullName;
use gix_hash::ObjectId;
use gix_object::{Commit, Find, Kind, Write, WriteTo as _};

use crate::error::{Error, Result};
use crate::merge::{Merge, three_way};
use crate::objects::{commit_tree, parents};

/// The two heads a merge reconciles onto one ref.
///
/// `ours` is the tip of the authorized side — the canonical ref the merge
/// tip will advance, or `None` when that ref does not exist yet (adopting a
/// contributor's brand-new entity onto a canonical ref that has no prior
/// tip). `theirs` is the head being folded in: the other machine's tip in a
/// divergence, or the contributor's inbox / self-run tip in an adoption.
#[derive(Debug, Clone)]
pub struct Heads {
    /// The ref the resulting merge tip advances; the gate recomputes this
    /// name from the merge's signed content (`gate.identity-binding`).
    pub refname: FullName,
    /// The authorized side's current tip, or `None` if the ref is new.
    pub ours: Option<ObjectId>,
    /// The head being folded in — always kept as a parent, never
    /// cherry-picked (`sync.adoption-no-cherry-pick`).
    pub theirs: ObjectId,
}

/// The result of [`merge_heads`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Merged {
    /// A signed merge tip that advances [`Heads::refname`]. It descends from
    /// both parents, so an authorized signature makes it satisfy the tip
    /// invariant (`sync.divergence-merge`); `theirs` is in its ancestry with
    /// attribution intact (`sync.adoption-no-cherry-pick`).
    Tip(ObjectId),
    /// The two heads changed the same leaf of the typed tree differently.
    /// Each path is a field (and, for a collection, an index) into the
    /// entity — a human resolves it before the merge can complete.
    Conflict(Vec<BString>),
}

/// Resolve two divergent heads into one signed merge tip, or report the
/// conflicting paths — the single machinery divergence and adoption share
/// (`sync.divergence-merge`, `sync.adoption-machinery`).
///
/// The typed trees of `ours` and `theirs` are merged three-way against
/// their merge base; a clean merge is recorded as a merge commit whose
/// parents are `[ours, theirs]` (just `[theirs]` when the canonical ref is
/// new), authored and committed by `author`, and signed by `sign`; the
/// merge names no ref of its own, and the gate recomputes the binding for
/// [`Heads::refname`] from the merged content. `sign` returns the
/// armored SSHSIG PEM for the commit's payload — exactly what git stores in
/// the `gpgsig` header — so the composition root injects the placing
/// member's key without this crate ever holding one.
///
/// Because `theirs` is always a parent, the contributor's original signed
/// commit stays in ancestry: this is a merge, never a cherry-pick
/// (`sync.adoption-no-cherry-pick`). The placer signs the *tip*, which is
/// what makes an authorized member's merge the legitimate adoption mechanism
/// (`gate.adoption-merge`) rather than a direct fast-forward to an
/// unauthorized signature (`gate.adoption-no-fast-forward`).
///
/// # Errors
///
/// Propagates object read/decode/write failures from the merge and from
/// building the commit.
///
/// # Examples
///
/// ```
/// use ents_model::{Provenance, namespace};
/// use ents_sync::resolve::{Heads, Merged, merge_heads};
/// use ents_testutil::{Keypair, MemRefStore, ObjectStore, enroll_member, write_meta_entity};
///
/// // A stand-in for `ents-forge`'s `Issue` (this crate cannot depend on
/// // `ents-forge`): any Facet-derived entity exercises the merge.
/// # #[derive(facet::Facet, Clone)]
/// # struct Issue { title: String, body: String, state: String }
/// #
/// let refs = MemRefStore::default();
/// let objects = ObjectStore::default();
/// let key = Keypair::from_seed(1);
/// enroll_member(&refs, &objects, "jdc", &key, Provenance::AdminRegistered, 100);
///
/// // Two of jdc's machines diverged on the same single-writer ref.
/// let name: gix::refs::FullName = "refs/meta/issues/1".try_into().expect("valid");
/// let issue = Issue {
///     title: "t".into(), body: "b".into(), state: "open".into(),
/// };
/// let ours = write_meta_entity(&refs, &objects, name.clone(), &issue, Some(&key), 200);
/// let mut other = issue.clone();
/// other.state = "closed".into();
/// let theirs = write_meta_entity(&refs, &objects, name.clone(), &other, Some(&key), 300);
///
/// let author = gix::actor::Signature {
///     name: "jdc".into(), email: "jdc@ents.test".into(),
///     time: gix::date::Time { seconds: 400, offset: 0 },
/// };
/// let heads = Heads { refname: name, ours: Some(ours), theirs };
/// let merged = merge_heads(&objects, &heads, &author, "Merge divergent heads",
///     |payload| key.sign(payload)).expect("merges");
/// assert!(matches!(merged, Merged::Tip(_)));
/// ```
// @relation(sync.divergence-merge, sync.adoption-machinery, sync.adoption-no-cherry-pick, scope=function)
pub fn merge_heads(
    objects: &(impl Find + Write),
    heads: &Heads,
    author: &gix::actor::Signature,
    summary: &str,
    sign: impl FnOnce(&[u8]) -> String,
) -> Result<Merged> {
    let theirs_tree = commit_tree(objects, heads.theirs)?;

    let (tree, parents) = match heads.ours {
        // Adopting onto a ref with no prior tip: nothing to merge, but
        // `theirs` still becomes the sole parent so attribution survives —
        // a degenerate merge, never a cherry-pick.
        None => (theirs_tree, vec![heads.theirs]),
        Some(ours) => {
            let base = merge_base(objects, ours, heads.theirs)?;
            let base_tree = base.map(|b| commit_tree(objects, b)).transpose()?;
            let ours_tree = commit_tree(objects, ours)?;
            match three_way(objects, base_tree, ours_tree, theirs_tree)? {
                Merge::Clean(tree) => (tree, vec![ours, heads.theirs]),
                Merge::Conflict(paths) => return Ok(Merged::Conflict(paths)),
            }
        }
    };

    let tip = seal(objects, tree, parents, author, summary, sign)?;
    Ok(Merged::Tip(tip))
}

/// Build and sign the merge commit — the tip whose signature, not any tree
/// content, is what satisfies the tip invariant. The commit names no ref
/// of its own; the gate recomputes the binding from the merged content and
/// the all-roots walk (`gate.identity-binding`), which holds across this
/// merge because both parents descend from the same genesis.
fn seal(
    objects: &impl Write,
    tree: ObjectId,
    parents: Vec<ObjectId>,
    author: &gix::actor::Signature,
    summary: &str,
    sign: impl FnOnce(&[u8]) -> String,
) -> Result<ObjectId> {
    let message = summary.to_owned();
    let mut commit = Commit {
        tree,
        parents: parents.into(),
        author: author.clone(),
        committer: author.clone(),
        encoding: None,
        message: message.into(),
        extra_headers: Vec::new(),
    };

    // Sign exactly as `git commit -S` does: SSHSIG over the commit
    // serialized *without* its gpgsig header, stored back as that header —
    // so the signature is repository data that verifies offline
    // (`gate.signature-artifact`).
    let mut payload = Vec::new();
    commit.write_to(&mut payload).map_err(|e| Error::Decode {
        oid: tree,
        detail: format!("serializing merge commit failed: {e}"),
    })?;
    let pem = sign(&payload);
    commit
        .extra_headers
        .push(("gpgsig".into(), pem.trim_end().into()));

    let mut raw = Vec::new();
    commit.write_to(&mut raw).map_err(|e| Error::Decode {
        oid: tree,
        detail: format!("serializing signed merge commit failed: {e}"),
    })?;
    Ok(objects.write_buf(Kind::Commit, &raw)?)
}

/// The nearest common ancestor of `a` and `b` by parent edges, or `None`
/// when they share no ancestor (each is then merged against an empty base).
///
/// This is a breadth-first nearest-ancestor search: it collects every
/// ancestor of `a`, then walks `b`'s ancestry breadth-first and returns the
/// first commit already seen from `a`. For the divergence and adoption
/// shapes sync produces — two lines splitting from one common tip — that is
/// the true merge base. It does not resolve the multiple-merge-base
/// criss-cross case optimally; a stricter base would only ever *reduce*
/// spurious conflicts, never admit a wrong clean merge, since the three-way
/// rule keeps a field only when at least one side matches the base.
fn merge_base(objects: &impl Find, a: ObjectId, b: ObjectId) -> Result<Option<ObjectId>> {
    let ancestors_of_a = ancestors(objects, a)?;
    let mut queue = std::collections::VecDeque::from([b]);
    let mut seen = HashSet::new();
    while let Some(oid) = queue.pop_front() {
        if !seen.insert(oid) {
            continue;
        }
        if ancestors_of_a.contains(&oid) {
            return Ok(Some(oid));
        }
        for parent in parents(objects, oid)? {
            queue.push_back(parent);
        }
    }
    Ok(None)
}

/// Every commit reachable from `oid` by parent edges, inclusive.
fn ancestors(objects: &impl Find, oid: ObjectId) -> Result<HashSet<ObjectId>> {
    let mut seen = HashSet::new();
    let mut stack = vec![oid];
    while let Some(oid) = stack.pop() {
        if !seen.insert(oid) {
            continue;
        }
        stack.extend(parents(objects, oid)?);
    }
    Ok(seen)
}
