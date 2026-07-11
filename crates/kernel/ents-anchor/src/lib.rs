//! Anchor storage and projection: durable pointers into source — a blob,
//! an optional line range, and a specific commit — and their read-time
//! projection onto any other commit.
//!
//! This crate owns the `Anchor` abstraction from `docs/spec/anchor.sdoc`
//! (overview.sdoc abstraction 3). Anchors resolve and project independently
//! of any consumer: `ents-forge`'s `Comment` is merely the first client
//! (its `anchor: RawTree` field embeds the tree an [`Anchor`] serializes
//! to), and reviews, TODO trackers, and blame overlays can reuse the same
//! mechanism. (`ents-forge` depends on this crate, not the other way
//! around, so this crate's own examples and tests stand a `Comment`-shaped
//! struct in for it rather than importing it.)
//!
//! # Spec coverage
//!
//! This crate implements, from `docs/spec/anchor.sdoc`:
//!
//! - `anchor.definition` — [`Anchor`] and [`capture`]'s validation.
//! - `anchor.immutable` — no mutating API exists; [`snippet`] derives the
//!   anchored text at read time; the commit id is plain data.
//! - `anchor.retention` — [`Anchor::content`] and [`Anchor::context`] are
//!   ordinary blob tree entries in the anchor's own serialized tree, never
//!   a gitlink.
//! - `anchor.projection` — [`project`] / [`project_exact`] and the
//!   four-outcome [`Projection`] taxonomy.
//! - `anchor.fuzzy-fallback` — [`project_from_context`], which [`project`]
//!   degrades to once the anchored commit has been garbage collected.
//!
//! # Examples
//!
//! Capture an anchor, store it inside a `Comment`, read it back, and
//! project it onto a later commit:
//!
//! ```
//! use ents_anchor::{Anchor, LineRange, Projection};
//! use facet_git_tree::RawTree;
//!
//! // Stands in for `ents-forge`'s `Comment` (this crate cannot
//! // depend on `ents-forge`, which itself depends on this crate): any
//! // struct embedding an anchor's tree by `RawTree` behaves identically.
//! # #[derive(facet::Facet)]
//! # struct Comment { body: String, anchor: RawTree }
//! #
//! # fn git(dir: &std::path::Path, args: &[&str]) {
//! #     let status = std::process::Command::new("git").arg("-C").arg(dir)
//! #         .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
//! #         .args(args).status().unwrap();
//! #     assert!(status.success());
//! # }
//! # let dir = tempfile::tempdir().expect("tempdir");
//! # std::process::Command::new("git").arg("init").arg("-q").arg(dir.path()).status().unwrap();
//! # std::fs::write(dir.path().join("file.txt"), (1..=10).map(|n| format!("line {n}\n")).collect::<String>()).unwrap();
//! # git(dir.path(), &["add", "-A"]);
//! # git(dir.path(), &["commit", "-q", "-m", "one"]);
//! let repo = gix::open(dir.path()).expect("open");
//!
//! // Capture against HEAD: commit, path, blob, and range are validated
//! // and recorded; content and context are embedded (`anchor.retention`).
//! let anchor = ents_anchor::capture(&repo, "HEAD", "file.txt", Some(LineRange { start: 3, end: 4 }))
//!     .expect("capture");
//! assert_eq!(ents_anchor::snippet(&anchor).expect("snippet"), "line 3\nline 4\n");
//!
//! // The anchor serializes into the same store the comment does; the
//! // comment embeds it by tree id (`RawTree`), so the anchored content is
//! // reachable from the comment's own ref.
//! let store = facet_git_tree::ObjectStore::default();
//! let anchor_tree = facet_git_tree::serialize_into(&anchor, &store).expect("serialize anchor");
//! let comment = Comment {
//!     body: "these two lines look off by one".to_owned(),
//!     anchor: RawTree::new(anchor_tree),
//! };
//! let root = facet_git_tree::serialize_into(&comment, &store).expect("serialize comment");
//!
//! // Read the comment back and recover the identical anchor.
//! let back: Comment = facet_git_tree::deserialize(&root, &store).expect("deserialize comment");
//! let anchor_back: Anchor =
//!     facet_git_tree::deserialize(&back.anchor.oid(), &store).expect("deserialize anchor");
//! assert_eq!(anchor_back, anchor);
//!
//! // Edit above the range and project: the anchor relocates, unmutated.
//! # std::fs::write(dir.path().join("file.txt"), format!("added\n{}", (1..=10).map(|n| format!("line {n}\n")).collect::<String>())).unwrap();
//! # git(dir.path(), &["add", "-A"]);
//! # git(dir.path(), &["commit", "-q", "-m", "two"]);
//! let repo = gix::open(dir.path()).expect("reopen");
//! assert_eq!(
//!     ents_anchor::project(&repo, &anchor_back, "HEAD").expect("project"),
//!     Projection::Relocated {
//!         path: "file.txt".to_owned(),
//!         lines: Some(LineRange { start: 4, end: 5 }),
//!     }
//! );
//! ```

mod anchor;
mod error;
#[cfg(test)]
mod fixture;
mod projection;
mod util;

pub use anchor::{Anchor, LineRange, capture, snippet};
pub use error::{Error, Result};
pub use projection::{Projection, project, project_exact, project_from_context};
