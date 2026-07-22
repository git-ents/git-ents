//! `sync`: remote synchronization for the forge — the one capability
//! `git ents` adds over the local primitives (`docs/spec/sync.adoc`).
//!
//! Sync fetches and pushes `refs/meta/*` and, crucially, turns the gate's
//! verdict into a decision the user acts on before pushing. Its single hard
//! responsibility is the schema-aware three-way merge over typed trees
//! (`crate::merge`); everything else — transfer, pre-flight, inbox routing —
//! is plumbing above traits that already exist by this phase (`RefStore`,
//! `Find`/`Write`, and `ents_gate::verify`). This crate never re-implements
//! the gate's judgment (it *calls* [`ents_gate::verify`], `gate.call-sites`)
//! and never writes to a ref except through the `RefStore` seam it is handed.
//!
//! Divergence resolution and adoption are deliberately *one* machinery
//! (`crate::resolve`), not two code paths (`sync.adoption-machinery`): a
//! member merging their own racing machines, a maintainer folding an inbox
//! entity onto its canonical ref, and a maintainer adopting a contributor's
//! self-run results all go through the same [`resolve::merge_heads`], which
//! keeps the folded-in head as a parent so its author's signature survives —
//! a merge, never a cherry-pick (`sync.adoption-no-cherry-pick`).
//!
//! # Spec coverage
//!
//! From `docs/spec/sync.adoc`:
//!
//! - `sync.forge-transfer` — [`transfer::fetch`], [`transfer::push`]: both
//!   copy each meta-ref's full object closure, commit objects verbatim, so
//!   history and signatures move with the ref.
//! - `sync.pre-flight` — [`preflight::preflight`]: the identical
//!   [`ents_gate::verify`] every call site runs, so a pre-flight verdict is
//!   a prediction that can only be stale (`gate.call-sites`).
//! - `sync.inbox-routing` — [`preflight::inbox_route`], surfaced by
//!   [`preflight::PreFlight::inbox`] and [`transfer::Pushed::Inbox`]: any
//!   negative advisory verdict offers the author's own inbox segment.
//! - `sync.divergence-merge` — [`merge::three_way`] and
//!   [`resolve::merge_heads`]: a schema-aware three-way merge whose tip,
//!   once signed, satisfies the tip invariant.
//! - `sync.adoption-machinery` — [`resolve::merge_heads`] is the *same*
//!   function divergence uses; adoption is only a different pair of heads.
//! - `sync.adoption-no-cherry-pick` — [`resolve::merge_heads`] always keeps
//!   `theirs` as a parent; it never re-authors the contributor's commit.
//! - `sync.local-advisory` — sync never blocks a local write on a verdict;
//!   the consequence it owns is the inbox offer ([`mod@preflight`],
//!   [`transfer::push`] is the sole place a verdict gates a *remote* write).
//!
//! # Examples
//!
//! A same-actor divergence — two of one member's machines each editing a
//! *different* field of the same issue — resolved into a signed merge tip
//! the gate then accepts (`sync.divergence-merge`).
//!
//! ```
//! use ents_gate::{Config, Update, Verdict, verify};
//! use ents_model::{Provenance, namespace};
//! use ents_sync::resolve::{Heads, Merged, merge_heads};
//! use ents_testutil::{
//!     CommitSpec, Keypair, MemRefStore, ObjectStore, enroll_member, write_commit, write_meta_entity,
//! };
//!
//! // A stand-in for `ents-forge`'s `Issue` (this crate cannot depend on
//! // `ents-forge`): any Facet-derived entity exercises the merge.
//! # #[derive(facet::Facet, Clone)]
//! # struct Issue { title: String, body: String, state: String }
//! #
//! let refs = MemRefStore::default();
//! let objects = ObjectStore::default();
//! let jdc = Keypair::from_seed(1);
//!
//! // Enroll (bootstrap) and turn verification on by setting the epoch.
//! enroll_member(&refs, &objects, "jdc", &jdc, Provenance::AdminRegistered, 100);
//! let config: gix::refs::FullName = namespace::CONFIG_REF.try_into().expect("valid");
//! write_meta_entity(&refs, &objects, config, &Config { epoch: Some(200), ..Config::default() }, Some(&jdc), 200);
//!
//! let issue = Issue {
//!     title: "t".into(), body: "b".into(), state: "open".into(),
//! };
//!
//! // A common base (the genesis), then two divergent children editing
//! // disjoint fields. The issue's id is the genesis commit's own oid
//! // (`meta-ref.identity-binding`), so the ref is named from it.
//! let base_tree = facet_git_tree::serialize_into(&issue, &objects).expect("ser");
//! let base = write_commit(&objects, &CommitSpec { tree: base_tree, parents: vec![], message: "Open".into(), seconds: 300 }, Some(&jdc));
//! let name: gix::refs::FullName = format!("refs/meta/issues/{base}").try_into().expect("valid");
//!
//! let mut renamed = issue.clone();
//! renamed.title = "renamed".into();
//! let ours_tree = facet_git_tree::serialize_into(&renamed, &objects).expect("ser");
//! let ours = write_commit(&objects, &CommitSpec { tree: ours_tree, parents: vec![base], message: "Rename".into(), seconds: 400 }, Some(&jdc));
//!
//! let mut closed = issue.clone();
//! closed.state = "closed".into();
//! let theirs_tree = facet_git_tree::serialize_into(&closed, &objects).expect("ser");
//! let theirs = write_commit(&objects, &CommitSpec { tree: theirs_tree, parents: vec![base], message: "Close".into(), seconds: 400 }, Some(&jdc));
//!
//! let author = gix::actor::Signature {
//!     name: "jdc".into(), email: "jdc@ents.test".into(),
//!     time: gix::date::Time { seconds: 500, offset: 0 },
//! };
//! let heads = Heads { refname: name.clone(), ours: Some(ours), theirs };
//! let Merged::Tip(tip) =
//!     merge_heads(&objects, &heads, &author, "Merge divergent heads", |p| jdc.sign(p)).expect("merges")
//! else { panic!("a same-actor divergence merges cleanly") };
//!
//! // The merged tree carries *both* disjoint edits — the schema-aware
//! // property this crate's tests pin field-by-field. Here we show the
//! // consequence that matters to sync: the merge tip satisfies the tip
//! // invariant, so the gate accepts it advancing the ref from `ours`.
//! let snapshot = refs.fetched_copy();
//! snapshot.set(name.as_ref(), ours);
//! let verdict = verify(&snapshot, &objects, &Update { name, new: Some(tip) }).expect("evaluates");
//! assert!(matches!(verdict, Verdict::Pass(_)));
//! ```

mod error;
mod objects;

pub mod merge;
pub mod preflight;
pub mod resolve;
pub mod transfer;

pub use error::{Error, Result};
pub use merge::{Merge, three_way};
pub use preflight::{PreFlight, inbox_route, preflight};
pub use resolve::{Heads, Merged, merge_heads};
pub use transfer::{Diverged, FetchReport, Pushed, fetch, push};
