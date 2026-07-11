//! Divergence and adoption over the one merge machinery
//! (`sync.divergence-merge`, `sync.adoption-machinery`,
//! `sync.adoption-no-cherry-pick`).
//!
//! Strategy: **integration harness** — these are enumerable end-to-end
//! scenarios whose point is that a real signed merge tip, built by
//! [`ents_sync::merge_heads`], is accepted by the *real* gate
//! ([`ents_gate::verify`]) and keeps the folded-in commit in ancestry. A
//! property test would not add coverage over the specific shapes the spec
//! names; the value is in exercising the same function the production path
//! runs against genuine git objects and signatures.

#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use ents_gate::{Config, Update, Verdict, verify};
use ents_model::trailer::Trailers;
use ents_model::{Provenance, namespace};
use ents_sync::{Heads, Merged, merge_heads};
use ents_testutil::{
    CommitSpec, Keypair, MemRefStore, ObjectStore, enroll_member, write_commit, write_meta_entity,
};
use gix::refs::FullName;
use gix_hash::ObjectId;

/// A stand-in for `ents-forge`'s `Issue` (this crate cannot depend on
/// `ents-forge`): any multi-field entity exercises the same divergence and
/// adoption machinery, which is generic over the typed tree.
#[derive(Debug, Clone, PartialEq, Eq, facet::Facet)]
struct Issue {
    title: String,
    body: String,
    state: String,
}

fn issue(state: &str) -> Issue {
    Issue {
        title: "t".into(),
        body: "b".into(),
        state: state.into(),
    }
}

/// Build a signed commit recording `entity`, bound to `refname`, with the
/// given parents — the general shape [`write_meta_entity`] specializes.
fn signed_commit(
    objects: &ObjectStore,
    refname: &FullName,
    entity: &Issue,
    parents: Vec<ObjectId>,
    key: &Keypair,
    seconds: i64,
) -> ObjectId {
    let tree = facet_git_tree::serialize_into(entity, objects).unwrap();
    let trailers = Trailers {
        ents_ref: Some(refname.clone()),
        schema_version: None,
    };
    let message = format!("Mutate {}\n\n{}", refname.as_bstr(), trailers.render());
    write_commit(
        objects,
        &CommitSpec {
            tree,
            parents,
            message,
            seconds,
        },
        Some(key),
    )
}

fn author(seconds: i64) -> gix::actor::Signature {
    gix::actor::Signature {
        name: "placer".into(),
        email: "placer@ents.test".into(),
        time: gix::date::Time { seconds, offset: 0 },
    }
}

/// Enroll `admin` and turn verification on by recording the epoch.
fn boot(refs: &MemRefStore, objects: &ObjectStore, admin: &Keypair) {
    enroll_member(
        refs,
        objects,
        "admin",
        admin,
        Provenance::AdminRegistered,
        100,
    );
    let config: FullName = namespace::CONFIG_REF.try_into().unwrap();
    write_meta_entity(
        refs,
        objects,
        config,
        &Config { epoch: Some(200) },
        Some(admin),
        200,
    );
}

fn parents_of(objects: &ObjectStore, tip: ObjectId) -> Vec<ObjectId> {
    match objects.get(&tip).expect("tip present") {
        gix::objs::Object::Commit(c) => c.parents.into_vec(),
        _ => panic!("merge tip is a commit"),
    }
}

/// Same-actor divergence: two of one member's machines edit disjoint fields
/// of the same single-writer ref. The merge folds both, and the merge tip
/// satisfies the tip invariant — the gate accepts it advancing the ref from
/// the canonical tip (`sync.divergence-merge`, `gate.same-actor-divergence`).
// @relation(sync.divergence-merge, scope=function, role=Verifies)
#[test]
fn same_actor_divergence_merges_to_a_gate_valid_tip() {
    let refs = MemRefStore::default();
    let objects = ObjectStore::default();
    let jdc = Keypair::from_seed(1);
    boot(&refs, &objects, &jdc);

    let name: FullName = "refs/meta/issues/1".try_into().unwrap();
    let base = signed_commit(&objects, &name, &issue("open"), vec![], &jdc, 300);

    // Two divergent children of the same base, editing different fields.
    let mut ours_issue = issue("open");
    ours_issue.title = "renamed".into();
    let ours = signed_commit(&objects, &name, &ours_issue, vec![base], &jdc, 400);
    let theirs = signed_commit(&objects, &name, &issue("closed"), vec![base], &jdc, 400);

    let heads = Heads {
        refname: name.clone(),
        ours: Some(ours),
        theirs,
    };
    let Merged::Tip(tip) = merge_heads(
        &objects,
        &heads,
        &author(500),
        "Merge divergent heads",
        |p| jdc.sign(p),
    )
    .unwrap() else {
        panic!("a same-actor divergence merges cleanly");
    };

    // Both disjoint edits survive the merge.
    let merged_tree = match objects.get(&tip).unwrap() {
        gix::objs::Object::Commit(c) => c.tree,
        _ => panic!("commit"),
    };
    let got: Issue = facet_git_tree::deserialize(&merged_tree, &objects).unwrap();
    assert_eq!(got.title, "renamed");
    assert_eq!(got.state, "closed");

    // The merge tip satisfies the tip invariant.
    let snapshot = refs.fetched_copy();
    snapshot.set(name.as_ref(), ours);
    let verdict = verify(
        &snapshot,
        &objects,
        &Update {
            name,
            new: Some(tip),
        },
    )
    .unwrap();
    assert!(matches!(verdict, Verdict::Pass(_)), "{verdict:?}");
}

/// Adoption of a contributor's brand-new entity onto a canonical ref that
/// has no prior tip: the maintainer merges the contributor's signed commit
/// (`sync.adoption-machinery`, `gate.adoption-merge`), which stays a parent
/// so its signature and attribution survive — a merge, never a cherry-pick
/// (`sync.adoption-no-cherry-pick`). The maintainer's signature on the tip
/// makes it satisfy the tip invariant.
// @relation(sync.adoption-machinery, sync.adoption-no-cherry-pick, scope=function, role=Verifies)
#[test]
fn adoption_merges_the_contributors_commit_without_cherry_picking() {
    let refs = MemRefStore::default();
    let objects = ObjectStore::default();
    let admin = Keypair::from_seed(1);
    let bob = Keypair::from_seed(2);
    boot(&refs, &objects, &admin);
    enroll_member(&refs, &objects, "bob", &bob, Provenance::SelfAttested, 250);

    // Bob submits an issue under his own inbox segment (all he may write).
    let inbox: FullName = "refs/meta/inbox/bob/issues/5".try_into().unwrap();
    let contribution = signed_commit(&objects, &inbox, &issue("open"), vec![], &bob, 300);

    // The maintainer adopts it onto the canonical ref via the *same*
    // machinery divergence uses — only the heads differ.
    let canonical: FullName = "refs/meta/issues/5".try_into().unwrap();
    let heads = Heads {
        refname: canonical.clone(),
        ours: None,
        theirs: contribution,
    };
    let Merged::Tip(tip) = merge_heads(&objects, &heads, &author(400), "Adopt bob's issue", |p| {
        admin.sign(p)
    })
    .unwrap() else {
        panic!("a trivial adoption merges cleanly");
    };

    // Not a cherry-pick: bob's original signed commit is in ancestry.
    assert!(
        parents_of(&objects, tip).contains(&contribution),
        "the contributor's commit must remain a parent, its signature intact"
    );

    // The maintainer's signature makes the tip satisfy the tip invariant on
    // the previously-absent canonical ref.
    let verdict = verify(
        &refs,
        &objects,
        &Update {
            name: canonical,
            new: Some(tip),
        },
    )
    .unwrap();
    assert!(matches!(verdict, Verdict::Pass(_)), "{verdict:?}");
}

/// Adoption folding an inbox entity onto an *existing* canonical ref rides
/// the identical [`merge_heads`] path, with a real three-way merge over the
/// two typed trees (`sync.adoption-machinery`).
// @relation(sync.adoption-machinery, scope=function, role=Verifies)
#[test]
fn adoption_onto_existing_canonical_ref_uses_the_merge() {
    let refs = MemRefStore::default();
    let objects = ObjectStore::default();
    let admin = Keypair::from_seed(1);
    let bob = Keypair::from_seed(2);
    boot(&refs, &objects, &admin);
    enroll_member(&refs, &objects, "bob", &bob, Provenance::SelfAttested, 250);

    let canonical: FullName = "refs/meta/issues/7".try_into().unwrap();
    let base = signed_commit(&objects, &canonical, &issue("open"), vec![], &admin, 300);

    // Bob branches from the canonical base and edits a field in his inbox.
    let inbox: FullName = "refs/meta/inbox/bob/issues/7".try_into().unwrap();
    let mut contributed = issue("open");
    contributed.title = "bob's title".into();
    let contribution = signed_commit(&objects, &inbox, &contributed, vec![base], &bob, 350);

    let heads = Heads {
        refname: canonical.clone(),
        ours: Some(base),
        theirs: contribution,
    };
    let Merged::Tip(tip) = merge_heads(&objects, &heads, &author(400), "Adopt bob's edit", |p| {
        admin.sign(p)
    })
    .unwrap() else {
        panic!("clean adoption");
    };

    assert!(parents_of(&objects, tip).contains(&contribution));
    let snapshot = refs.fetched_copy();
    snapshot.set(canonical.as_ref(), base);
    let verdict = verify(
        &snapshot,
        &objects,
        &Update {
            name: canonical,
            new: Some(tip),
        },
    )
    .unwrap();
    assert!(matches!(verdict, Verdict::Pass(_)), "{verdict:?}");
}
