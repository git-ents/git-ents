//! Typed documents on `refs/meta/*` refs, stored as git object graphs.
//!
//! A [`Store`] reads and writes [`Facet`] values to a ref's tree through
//! [`facet_git_tree`]: the value becomes a git tree, the tree is wrapped in a
//! commit parented on the ref's prior tip, and the ref is moved to it. The
//! commit chain is the document's history and each commit's date is its
//! timestamp, so nothing about versioning has to be modeled in the tree
//! itself. This is the single home for the plumbing that the signer set, the
//! check set, and the run log all share.
//!
//! # Format stability
//!
//! A document's [`Facet`] shape *is* its on-disk format: the tree git holds is
//! derived from it, so an incompatible change (a renamed field, a changed
//! field type) silently stops reading data already on a ref and only surfaces
//! at load time. The policy is therefore to never change a meta-ref document
//! type incompatibly; each type carries a load test against a hand-built
//! fixture in the real layout to catch a regression at compile-and-test time
//! rather than in production.
//!
//! Every tree-rooted document also carries a one-blob `.schema` entry at its
//! root (see [`SchemaVersion`]), a sibling of the document's own fields
//! naming the shape's on-disk version. A tree missing the entry is version 1
//! — the pre-marker format, which must keep reading fine — and a tree naming
//! a version newer than the binary's [`SchemaVersion::VERSION`] fails with
//! [`Error::UnsupportedSchema`] rather than however far a mismatched decode
//! happens to get. Migrating is then just a normal write: a new tree
//! committed on the ref's old tip, no separate migration step.

use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::path::Path;

use facet::Facet;
use gix::ObjectId;
use gix::objs::{Commit, FindExt as _, Write as _};
use gix::refs::Target;
use gix::refs::transaction::PreviousValue;

pub mod component;
mod merge;
mod schema;

pub use schema::SchemaVersion;

/// The author and committer identity stamped on every write, fixed so a write
/// is self-contained and independent of any ambient git config.
const IDENTITY_NAME: &str = "git-ents";
/// The email paired with [`IDENTITY_NAME`].
const IDENTITY_EMAIL: &str = "git-ents@localhost";

/// A failure opening the store or reading or writing one of its refs.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The repository could not be opened.
    #[error("could not open the repository")]
    Open(#[from] Box<gix::open::Error>),
    /// The repository's object database could not be opened.
    #[error("could not open the repository object database")]
    Odb,
    /// A document could not be (de)serialized from its git tree.
    #[error("could not (de)serialize the document: {0}")]
    Facet(#[from] facet_git_tree::Error),
    /// A ref could not be read, listed, or updated.
    #[error("git ref operation failed: {0}")]
    Ref(String),
    /// A git object could not be read or written.
    #[error("git object operation failed: {0}")]
    Object(String),
    /// A concurrent writer moved the ref since this write's snapshot was
    /// read, and either there was no common ancestor to merge from or the
    /// structural merge found the same leaf changed on both sides.
    #[error("conflicting concurrent write to the ref")]
    Conflict,
    /// A collection key (an id passed to [`Store::store_item`], a value's
    /// [`HasId::id`], or a map key given to [`Store::store_map`]) is unsafe
    /// as a ref path segment or tree entry name.
    #[error(
        "{0:?} is not a valid collection key: expected 1-64 ASCII alphanumerics, '.', '_', '-', or ':', not starting with '.'"
    )]
    InvalidKey(String),
    /// A document failed a domain invariant checked before the write (e.g. a
    /// member's validity window is inverted). Distinct from
    /// [`Error::InvalidKey`], which is about the collection key's ref-safety
    /// rather than the document's own content.
    #[error("{0}")]
    Invalid(String),
    /// A document's tree names a `.schema` version newer than this binary's
    /// [`SchemaVersion::VERSION`] for the type — a future format this binary
    /// was never taught to read, reported cleanly rather than surfacing as a
    /// decode failure.
    #[error("schema {found}, this binary reads {supported}")]
    UnsupportedSchema {
        /// The version named by the tree's `.schema` marker.
        found: u32,
        /// The newest version this binary's copy of the type supports.
        supported: u32,
    },
}

/// Whether `segment` is safe as a single ref-path or tree-entry segment: one
/// to sixty-four ASCII alphanumerics, `.`, `_`, `-`, or `:`, not starting with
/// `.`, and never containing `/`. Every collection key (a member's principal,
/// a check's name, a revoked key's colon-form MD5 fingerprint, an issue's
/// genesis hash) is stored as exactly one such segment, so an unchecked key
/// could otherwise inject an extra path component or collide with a sibling
/// entry. `:` is allowed — narrower than a repository path segment
/// (`namespace.path`) — because the MD5 fingerprint form the specification
/// requires (`cli.key-resolution`) is colon-separated.
#[must_use]
pub fn ref_segment_ok(segment: &str) -> bool {
    !segment.is_empty()
        && segment.len() <= 64
        && !segment.starts_with('.')
        && segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b':'))
}

/// A document that legitimately stores its own collection key.
///
/// Most decomposed-ref collections must *not* implement this: an issue's
/// stable key is its ref's genesis hash, never a stored field, and duplicating
/// it inside the document would let the stored copy disagree with the ref. A
/// member is the exception — its principal is both the ref segment and a
/// field genuinely used elsewhere (rendering `allowed_signers`) — so `HasId`
/// lets a caller store one without passing the principal twice.
pub trait HasId {
    /// The value's collection key — the segment its ref is stored under.
    fn id(&self) -> &str;
}

/// The content-addressed object id `value` would serialize to, as a hex
/// string — computed against a throwaway in-memory object store, so it is
/// available before deciding whether (or where) a repository should hold it.
/// A genesis key (an issue's or a comment's stable id) is exactly this: the
/// hash of the content that originates it, needing no counter and no ref to
/// already exist.
pub fn content_hash<T: for<'a> Facet<'a>>(value: &T) -> Result<String, Error> {
    let (oid, _store) = facet_git_tree::serialize(value)?;
    Ok(oid.to_string())
}

/// Derive a document's stable genesis key: `origin`'s object id (hex) when the
/// document derives from one — one origin, one document, deduplicated on
/// provenance — otherwise its own [`content_hash`], since every document is a
/// git object and so always has one. The key is computed once and never
/// renamed; cross-references key off it.
pub fn new_id<T: for<'a> Facet<'a>>(origin: Option<&str>, content: &T) -> Result<String, Error> {
    match origin {
        Some(origin) => Ok(origin.to_owned()),
        None => content_hash(content),
    }
}

// @relation(storage.meta-ref, nonfunctional.object-store)
/// A repository's typed `refs/meta/*` store.
///
/// Refs are read and updated through the high-level [`gix`] API, while all
/// object IO uses an object database opened on the *common* git directory
/// rather than `--git-path objects`: inside a hook git points the latter at a
/// receive-pack quarantine holding only the incoming pack, while the documents
/// we read and write live in the durable store.
pub struct Store {
    repo: gix::Repository,
    odb: gix::odb::Handle,
}

impl Store {
    // @relation(nonfunctional.object-store)
    /// Open the typed store for the repository at `repo`.
    pub fn open(repo: &Path) -> Result<Self, Error> {
        let repo = gix::open(repo).map_err(|error| Error::Open(Box::new(error)))?;
        let odb = gix::odb::at(repo.common_dir().join("objects")).map_err(|_io| Error::Odb)?;
        Ok(Self { repo, odb })
    }

    /// Load the document on `refname`, or `None` when the ref is absent.
    pub fn load<T: for<'a> Facet<'a> + SchemaVersion>(
        &self,
        refname: &str,
    ) -> Result<Option<T>, Error> {
        let Some(commit) = self.ref_commit(refname)? else {
            return Ok(None);
        };
        let tree = self.read_commit(&commit)?.tree;
        let (tree, version) = schema::strip(&self.odb, tree)?;
        schema::check::<T>(version)?;
        Ok(Some(facet_git_tree::deserialize(&tree, &self.odb)?))
    }

    /// Write `value` to `refname` as a new commit on top of the ref's current
    /// tip, so the update fast-forwards and accrues history.
    ///
    /// Uses compare-and-swap: if a concurrent writer moved the ref first,
    /// this attempts a schema-aware structural merge of the two documents
    /// against their common base and retries, up to [`MAX_MERGE_RETRIES`]
    /// times, before giving up with [`Error::Conflict`].
    ///
    /// ## Requirements
    ///
    /// @relation(storage.concurrency)
    pub fn store<T: for<'a> Facet<'a> + SchemaVersion>(
        &self,
        refname: &str,
        value: &T,
        message: &str,
    ) -> Result<(), Error> {
        self.store_impl(refname, value, message, None, &[])
    }

    /// Like [`store`](Self::store), but also parenting the written commit on
    /// each of `extra_parents` (beyond the ref's own prior tip), so the
    /// commit stays reachability-anchored to other history it depends on —
    /// e.g. a comment's commit carries the commit it annotates as a second
    /// parent, so the annotated commit can never be garbage-collected out
    /// from under it.
    pub fn store_with_parents<T: for<'a> Facet<'a> + SchemaVersion>(
        &self,
        refname: &str,
        value: &T,
        message: &str,
        extra_parents: &[ObjectId],
    ) -> Result<(), Error> {
        self.store_impl(refname, value, message, None, extra_parents)
    }

    /// Like [`store`](Self::store), but attributing authorship to `author`
    /// (a `(name, email)` pair) while the committer stays the git-ents system
    /// identity — the way a web edit records the human who made the change while
    /// the server is the committer.
    pub fn store_authored<T: for<'a> Facet<'a> + SchemaVersion>(
        &self,
        refname: &str,
        value: &T,
        message: &str,
        author: (&str, &str),
    ) -> Result<(), Error> {
        self.store_impl(refname, value, message, Some(author), &[])
    }

    /// Like [`store_authored`](Self::store_authored), but also parenting the
    /// written commit on each of `extra_parents`, per
    /// [`store_with_parents`](Self::store_with_parents).
    pub fn store_authored_with_parents<T: for<'a> Facet<'a> + SchemaVersion>(
        &self,
        refname: &str,
        value: &T,
        message: &str,
        author: (&str, &str),
        extra_parents: &[ObjectId],
    ) -> Result<(), Error> {
        self.store_impl(refname, value, message, Some(author), extra_parents)
    }

    /// ## Requirements
    ///
    /// @relation(storage.concurrency)
    fn store_impl<T: for<'a> Facet<'a> + SchemaVersion>(
        &self,
        refname: &str,
        value: &T,
        message: &str,
        author: Option<(&str, &str)>,
        extra_parents: &[ObjectId],
    ) -> Result<(), Error> {
        let mut expected = self.ref_commit(refname)?;
        // Bare (unmarked) tree throughout: the `.schema` marker is added only
        // at the point of writing, so a retry's structural merge never has to
        // know about it.
        let mut tree = facet_git_tree::serialize_into(value, &self.odb)?;
        for _ in 0..=MAX_MERGE_RETRIES {
            let versioned = schema::inject(&self.odb, tree, T::VERSION)?;
            let parents = expected
                .into_iter()
                .chain(extra_parents.iter().copied())
                .collect();
            let commit = self.write_commit(versioned, parents, expected, message, author)?;
            match self.try_set_ref(refname, expected, commit) {
                Ok(()) => return Ok(()),
                Err(Error::Conflict) => {
                    // No common ancestor (two independent geneses racing to
                    // create the same ref) can't be merged; fail closed.
                    let Some(base) = expected else {
                        return Err(Error::Conflict);
                    };
                    let theirs = self.ref_commit(refname)?.ok_or(Error::Conflict)?;
                    let (base_tree, base_version) =
                        schema::strip(&self.odb, self.read_commit(&base)?.tree)?;
                    let (theirs_tree, theirs_version) =
                        schema::strip(&self.odb, self.read_commit(&theirs)?.tree)?;
                    schema::check::<T>(base_version)?;
                    schema::check::<T>(theirs_version)?;
                    tree = merge::three_way_merge::<T>(base_tree, tree, theirs_tree, &self.odb)?;
                    expected = Some(theirs);
                }
                Err(error) => return Err(error),
            }
        }
        Err(Error::Conflict)
    }

    /// Write `value` to `refname` in place, replacing the ref's tip commit
    /// (re-parented on the tip's own parents) rather than appending. Lets a
    /// single document advance through intermediate states without a commit per
    /// transition. When the ref is absent this starts a fresh history.
    ///
    /// Uses compare-and-swap on the tip this call read; a race is a state
    /// machine advancing twice from the same state, so it fails closed with
    /// [`Error::Conflict`] rather than merging — merging stale state could
    /// resurrect a dead outcome.
    ///
    /// ## Requirements
    ///
    /// @relation(storage.concurrency)
    pub fn amend<T: for<'a> Facet<'a> + SchemaVersion>(
        &self,
        refname: &str,
        value: &T,
        message: &str,
    ) -> Result<(), Error> {
        let tree = facet_git_tree::serialize_into(value, &self.odb)?;
        let tree = schema::inject(&self.odb, tree, T::VERSION)?;
        let expected = self.ref_commit(refname)?;
        let (parents, chain_parent) = match &expected {
            Some(tip) => {
                let tip = self.read_commit(tip)?;
                (tip.parents, tip.chain_parent)
            }
            None => (Vec::new(), None),
        };
        let commit = self.write_commit(tree, parents, chain_parent, message, None)?;
        self.try_set_ref(refname, expected, commit)
    }

    /// Write `tree` to `refname` as a new commit on top of the ref's current
    /// tip, so the update fast-forwards and accrues history — the same
    /// compare-and-swap shape [`store`](Self::store) uses, but for a caller
    /// that already has a tree of its own construction (not a [`Facet`]
    /// value to serialize), e.g. a `git-toolchain` import.
    pub fn store_tree(&self, refname: &str, tree: ObjectId, message: &str) -> Result<(), Error> {
        let expected = self.ref_commit(refname)?;
        let parents = expected.into_iter().collect();
        let commit = self.write_commit(tree, parents, expected, message, None)?;
        self.try_set_ref(refname, expected, commit)
    }

    /// Write `tree` to `refname` as a parentless commit, fully replacing the
    /// ref's prior tip. Like [`store_tree`], it uses a compare-and-swap shape
    /// for consistency, but omits the parent chain. This is suited for refs
    /// whose history has no audit value — e.g. cache snapshots — where old
    /// commits become unreachable and garbage-collectable rather than pinned
    /// by a parent chain.
    pub fn store_tree_replace(
        &self,
        refname: &str,
        tree: ObjectId,
        message: &str,
    ) -> Result<(), Error> {
        let expected = self.ref_commit(refname)?;
        let parents = Vec::new();
        let commit = self.write_commit(tree, parents, expected, message, None)?;
        self.try_set_ref(refname, expected, commit)
    }

    /// The tree of `refname`'s tip commit.
    pub fn ref_tree(&self, refname: &str) -> Result<ObjectId, Error> {
        let commit = self
            .ref_commit(refname)?
            .ok_or_else(|| Error::Ref(format!("{refname} does not exist")))?;
        Ok(self.read_commit(&commit)?.tree)
    }

    /// Delete `refname` outright, for a collection item removed by name
    /// rather than by rewriting a map document (e.g. `git-toolchain`'s
    /// `remove`).
    pub fn delete_ref(&self, refname: &str) -> Result<(), Error> {
        let reference = self
            .repo
            .find_reference(refname)
            .map_err(|error| Error::Ref(error.to_string()))?;
        reference
            .delete()
            .map_err(|error| Error::Ref(error.to_string()))
    }

    /// Load the item `id` under the collection ref namespace `prefix`
    /// (`{prefix}/{id}`), or `None` when its ref is absent. The thin wrapper
    /// every decomposed-ref collection (members, checks, issues, comments, …)
    /// shares instead of hand-formatting its own ref name.
    pub fn load_item<T: for<'a> Facet<'a>>(
        &self,
        prefix: &str,
        id: &str,
    ) -> Result<Option<T>, Error> {
        self.load(&format!("{prefix}/{id}"))
    }

    /// Store `value` as item `id` under the collection ref namespace `prefix`
    /// (`{prefix}/{id}`), inheriting [`store`](Self::store)'s CAS-and-merge
    /// behavior. Rejects `id` per [`ref_segment_ok`] before writing anything,
    /// since it becomes the ref's last path segment.
    pub fn store_item<T: for<'a> Facet<'a>>(
        &self,
        prefix: &str,
        id: &str,
        value: &T,
        message: &str,
    ) -> Result<(), Error> {
        self.store(&item_ref(prefix, id)?, value, message)
    }

    /// Like [`store_item`](Self::store_item), but attributing authorship to
    /// `author` the way [`store_authored`](Self::store_authored) does — for a
    /// collection whose documents recover their author from the ref's commits
    /// instead of storing one in the tree.
    pub fn store_item_authored<T: for<'a> Facet<'a>>(
        &self,
        prefix: &str,
        id: &str,
        value: &T,
        message: &str,
        author: (&str, &str),
    ) -> Result<(), Error> {
        self.store_authored(&item_ref(prefix, id)?, value, message, author)
    }

    /// Like [`store_item_authored`](Self::store_item_authored), but also
    /// parenting the written commit on each of `extra_parents`, per
    /// [`store_with_parents`](Self::store_with_parents).
    pub fn store_item_authored_with_parents<T: for<'a> Facet<'a>>(
        &self,
        prefix: &str,
        id: &str,
        value: &T,
        message: &str,
        author: (&str, &str),
        extra_parents: &[ObjectId],
    ) -> Result<(), Error> {
        self.store_authored_with_parents(
            &item_ref(prefix, id)?,
            value,
            message,
            author,
            extra_parents,
        )
    }

    /// Like [`store_item`](Self::store_item), but for a [`HasId`] value that
    /// carries its own collection key, so the caller does not pass it twice.
    pub fn store_keyed<T: for<'a> Facet<'a> + HasId>(
        &self,
        prefix: &str,
        value: &T,
        message: &str,
    ) -> Result<(), Error> {
        self.store_item(prefix, value.id(), value, message)
    }

    /// Load the scalar-keyed map document on `refname` as its flattened
    /// `Item` list, via `assemble` (the map key plus its `Body`, joined back
    /// into the public item that names it once instead of storing it twice).
    /// An absent ref yields an empty list.
    ///
    /// The one conversion every "named entries on a single ref" collection
    /// (checks, revocations, run outcomes) needs, done once rather than by a
    /// hand-written wrapper document per module.
    pub fn load_map<Body: for<'a> Facet<'a>, Item>(
        &self,
        refname: &str,
        assemble: impl Fn(String, Body) -> Item,
    ) -> Result<Vec<Item>, Error> {
        Ok(self
            .load::<BTreeMap<String, Body>>(refname)?
            .unwrap_or_default()
            .into_iter()
            .map(|(key, body)| assemble(key, body))
            .collect())
    }

    /// Replace the scalar-keyed map document on `refname` with `items`, via
    /// `split` (an item back to its map key and `Body`). Rejects any key that
    /// fails [`ref_segment_ok`] before writing anything.
    pub fn store_map<Body: for<'a> Facet<'a>, Item>(
        &self,
        refname: &str,
        items: &[Item],
        split: impl Fn(&Item) -> (String, Body),
        message: &str,
    ) -> Result<(), Error> {
        let doc: BTreeMap<String, Body> = items.iter().map(split).collect();
        for key in doc.keys() {
            if !ref_segment_ok(key) {
                return Err(Error::InvalidKey(key.clone()));
            }
        }
        self.store(refname, &doc, message)
    }

    /// Every item under the collection ref namespace `prefix`, paired with the
    /// id (the ref's last path segment) it was stored under, newest first.
    pub fn list_items<T: for<'a> Facet<'a>>(
        &self,
        prefix: &str,
    ) -> Result<Vec<(String, T)>, Error> {
        let mut items = Vec::new();
        for refname in self.list(&format!("{prefix}/"))? {
            let id = refname.rsplit('/').next().unwrap_or(&refname).to_owned();
            if let Some(value) = self.load::<T>(&refname)? {
                items.push((id, value));
            }
        }
        Ok(items)
    }

    /// The documents on `refname`'s commit chain as `(committer date, value)`
    /// pairs, newest first — one entry per commit, following first parents.
    ///
    /// Stops (without erroring) at the first commit that predates an
    /// incompatible format change to `T`: such a commit is unreadable forever,
    /// not transiently, so returning the readable prefix beats letting one
    /// stale commit blank out every newer entry that parses fine. A real I/O
    /// or repository-corruption error still propagates — including a `.schema`
    /// marker newer than `T::VERSION`, which names a real, actionable problem
    /// (this binary needs upgrading) rather than the shape drift the
    /// stop-on-first-miss rule is built for.
    pub fn history<T: for<'a> Facet<'a> + SchemaVersion>(
        &self,
        refname: &str,
    ) -> Result<Vec<(u64, T)>, Error> {
        let mut out = Vec::new();
        let mut cursor = self.ref_commit(refname)?;
        while let Some(oid) = cursor {
            let commit = self.read_commit(&oid)?;
            let (tree, version) = schema::strip(&self.odb, commit.tree)?;
            schema::check::<T>(version)?;
            match facet_git_tree::deserialize(&tree, &self.odb) {
                Ok(value) => out.push((commit.seconds, value)),
                Err(facet_git_tree::Error::Message(_)) => break,
                Err(error) => return Err(error.into()),
            }
            cursor = commit.chain_parent;
        }
        Ok(out)
    }

    /// Who created and who last updated the document on `refname`, recovered
    /// from the ref's commit chain — the genesis commit's author and the tip
    /// commit's author, following first parents — or `None` when the ref is
    /// absent. The commit *is* the document's provenance: a collection that
    /// writes through [`store_authored`](Self::store_authored) never stores an
    /// author or timestamp field in its tree, so neither can disagree with the
    /// history that actually produced it.
    pub fn provenance(&self, refname: &str) -> Result<Option<Provenance>, Error> {
        let Some(tip) = self.ref_commit(refname)? else {
            return Ok(None);
        };
        let mut commit = self.read_commit(&tip)?;
        let updated = commit.author.clone();
        while let Some(parent) = commit.chain_parent {
            commit = self.read_commit(&parent)?;
        }
        Ok(Some(Provenance {
            created: commit.author,
            updated,
        }))
    }

    /// [`provenance`](Self::provenance) for the item `id` under the collection
    /// ref namespace `prefix` (`{prefix}/{id}`).
    pub fn item_provenance(&self, prefix: &str, id: &str) -> Result<Option<Provenance>, Error> {
        self.provenance(&format!("{prefix}/{id}"))
    }

    /// The full names of the refs under `prefix`, newest committer date first.
    pub fn list(&self, prefix: &str) -> Result<Vec<String>, Error> {
        let platform = self
            .repo
            .references()
            .map_err(|error| Error::Ref(error.to_string()))?;
        let iter = platform
            .prefixed(prefix)
            .map_err(|error| Error::Ref(error.to_string()))?;
        let mut refs = Vec::new();
        for reference in iter {
            let mut reference = reference.map_err(|error| Error::Ref(error.to_string()))?;
            let name = reference.name().as_bstr().to_string();
            let oid = reference
                .peel_to_id()
                .map_err(|error| Error::Ref(error.to_string()))?
                .detach();
            refs.push((self.read_commit(&oid)?.seconds, name));
        }
        refs.sort_by_key(|(seconds, _name)| Reverse(*seconds));
        Ok(refs.into_iter().map(|(_seconds, name)| name).collect())
    }

    /// Resolve `refname` to the object id of its commit, or `None` when absent.
    fn ref_commit(&self, refname: &str) -> Result<Option<ObjectId>, Error> {
        match self
            .repo
            .try_find_reference(refname)
            .map_err(|error| Error::Ref(error.to_string()))?
        {
            Some(mut reference) => {
                let id = reference
                    .peel_to_id()
                    .map_err(|error| Error::Ref(error.to_string()))?;
                Ok(Some(id.detach()))
            }
            None => Ok(None),
        }
    }

    /// Read `oid`'s tree, parents, chain parent, author, and committer date
    /// from the durable store.
    fn read_commit(&self, oid: &ObjectId) -> Result<CommitFacts, Error> {
        let mut buffer = Vec::new();
        let commit = self
            .odb
            .find_commit(oid, &mut buffer)
            .map_err(|error| Error::Object(error.to_string()))?;
        let seconds = commit
            .committer()
            .map_err(|error| Error::Object(error.to_string()))?
            .seconds();
        let author = commit
            .author()
            .map_err(|error| Error::Object(error.to_string()))?;
        let chain_parent = commit
            .extra_headers()
            .find(CHAIN_PARENT_HEADER)
            .map(|value| {
                if value.is_empty() {
                    Ok(None)
                } else {
                    ObjectId::from_hex(value)
                        .map(Some)
                        .map_err(|error| Error::Object(error.to_string()))
                }
            })
            .transpose()?
            .unwrap_or_else(|| commit.parents().next());
        Ok(CommitFacts {
            tree: commit.tree(),
            parents: commit.parents().collect(),
            chain_parent,
            seconds: u64::try_from(seconds).unwrap_or(0),
            author: Authorship {
                name: author.name.to_string(),
                email: author.email.to_string(),
                seconds: u64::try_from(author.seconds()).unwrap_or(0),
            },
        })
    }

    /// Wrap `tree` in a commit over `parents` and write it to the durable
    /// store, recording `chain_parent` (the document's own prior state, as
    /// opposed to any other parent riding along for reachability, e.g. an
    /// anchored commit) in a header when `parents` holds more than just it —
    /// otherwise the chain and the parent list agree and no header is needed.
    /// The committer is always the git-ents system identity; `author`
    /// overrides the authorship when set, otherwise it too is the system
    /// identity.
    fn write_commit(
        &self,
        tree: ObjectId,
        parents: Vec<ObjectId>,
        chain_parent: Option<ObjectId>,
        message: &str,
        author: Option<(&str, &str)>,
    ) -> Result<ObjectId, Error> {
        let time = gix::date::Time::now_utc();
        let committer = gix::actor::Signature {
            name: IDENTITY_NAME.into(),
            email: IDENTITY_EMAIL.into(),
            time,
        };
        let author = match author {
            Some((name, email)) => gix::actor::Signature {
                name: name.into(),
                email: email.into(),
                time,
            },
            None => committer.clone(),
        };
        // The chain-parent header is only needed when the plain "first
        // parent is the chain" convention would recover the wrong thing —
        // i.e. a genesis commit (no chain parent) that still carries an
        // extra parent, which would otherwise occupy the first slot.
        let extra_headers = if parents.first().copied() == chain_parent {
            Vec::new()
        } else {
            let value = chain_parent.map(|oid| oid.to_string()).unwrap_or_default();
            vec![(CHAIN_PARENT_HEADER.into(), value.into())]
        };
        let commit = Commit {
            tree,
            parents: parents.into(),
            author,
            committer,
            encoding: None,
            message: message.into(),
            extra_headers,
        };
        self.odb
            .write(&commit)
            .map_err(|error| Error::Object(error.to_string()))
    }

    /// Point `refname` at `commit`, requiring its current tip to match
    /// `expected` (`None` meaning the ref must not yet exist). Fails with
    /// [`Error::Conflict`] specifically when the ref moved since `expected`
    /// was read, distinguishing a genuine race from any other ref-transaction
    /// failure.
    fn try_set_ref(
        &self,
        refname: &str,
        expected: Option<ObjectId>,
        commit: ObjectId,
    ) -> Result<(), Error> {
        let constraint = match expected {
            Some(oid) => PreviousValue::MustExistAndMatch(Target::Object(oid)),
            None => PreviousValue::MustNotExist,
        };
        match self
            .repo
            .reference(refname, commit, constraint, "git-ents: update")
        {
            Ok(_reference) => Ok(()),
            Err(error) => match self.ref_commit(refname) {
                Ok(current) if current == expected => Err(Error::Ref(error.to_string())),
                Ok(_current) => Err(Error::Conflict),
                Err(error) => Err(error),
            },
        }
    }
}

/// How many times [`Store::store_impl`] retries a merge-and-CAS round before
/// giving up with [`Error::Conflict`]. Bounds retry under sustained
/// contention; ordinary racing writers resolve within one or two rounds.
const MAX_MERGE_RETRIES: usize = 5;

/// The commit header recording a document's chain parent explicitly, written
/// only when the plain "first parent is the chain" convention would recover
/// the wrong thing: a genesis commit (no prior document state) that still
/// carries an extra parent for reachability (<<anchor.reachability>>), which
/// would otherwise occupy the first — and only — parent slot. An empty value
/// means the chain has no parent at all (this commit is the genesis).
const CHAIN_PARENT_HEADER: &str = "chain-parent";

/// The ref name for item `id` under the collection namespace `prefix`
/// (`{prefix}/{id}`), rejecting an `id` that fails [`ref_segment_ok`] since it
/// becomes the ref's last path segment.
fn item_ref(prefix: &str, id: &str) -> Result<String, Error> {
    if !ref_segment_ok(id) {
        return Err(Error::InvalidKey(id.to_owned()));
    }
    Ok(format!("{prefix}/{id}"))
}

/// An author identity and date read off one of a document ref's commits —
/// recovered from the commit header rather than stored in the document tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authorship {
    /// The author's name.
    pub name: String,
    /// The author's email.
    pub email: String,
    /// The author date, in seconds since the epoch.
    pub seconds: u64,
}

/// Who created and who last updated a document, per its ref's commit chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    /// The genesis commit's authorship — who created the document.
    pub created: Authorship,
    /// The tip commit's authorship — who last updated the document.
    pub updated: Authorship,
}

/// The facts read off a commit: its tree, its parents, its chain parent (the
/// document's own prior state, distinct from any other parent riding along
/// for reachability), its author, and its committer date.
struct CommitFacts {
    tree: ObjectId,
    parents: Vec<ObjectId>,
    chain_parent: Option<ObjectId>,
    seconds: u64,
    author: Authorship,
}

/// Shared git test fixtures: a throwaway repository and the plumbing helpers
/// the workspace's test suites drive it with, kept here once instead of as a
/// copy per crate. Compiled for this crate's own tests and under the
/// `test-support` feature, which downstream crates enable from their
/// dev-dependencies.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    #![allow(clippy::unwrap_used, reason = "test fixture")]

    use std::io::Write as _;
    use std::path::Path;
    use std::process::{Command, Stdio};

    /// A fresh temporary directory holding an initialized git repository.
    #[must_use]
    pub fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let status = Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["init", "-q"])
            .status()
            .unwrap();
        assert!(status.success());
        for (key, value) in [("user.email", "test@example.com"), ("user.name", "test")] {
            let status = Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(["config", key, value])
                .status()
                .unwrap();
            assert!(status.success());
        }
        dir
    }

    /// Stage everything in `dir` and commit it as `message` under the fixed
    /// test identity.
    pub fn commit_all(dir: &Path, message: &str) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["add", "-A"])
            .status()
            .unwrap();
        assert!(status.success());
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-q",
                "-m",
                message,
            ])
            .status()
            .unwrap();
        assert!(status.success());
    }

    /// The full hex id of `dir`'s `HEAD` commit.
    #[must_use]
    pub fn head(dir: &Path) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    /// Run git in `repo` with `input` on stdin, returning its trimmed stdout.
    #[must_use]
    pub fn git_with_stdin(repo: &Path, args: &[&str], input: &str) -> String {
        let mut child = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "git {args:?} failed");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::let_underscore_must_use,
        reason = "unit test"
    )]

    use std::collections::BTreeMap;

    use super::*;
    use crate::test_support::repo;

    fn entries(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn absent_ref_loads_none() {
        let dir = repo();
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.load::<String>("refs/meta/bag").unwrap(), None);
    }

    #[test]
    fn store_then_load_round_trips() {
        let dir = repo();
        let store = Store::open(dir.path()).unwrap();
        store
            .store("refs/meta/bag", &"first".to_owned(), "write")
            .unwrap();
        assert_eq!(
            store.load::<String>("refs/meta/bag").unwrap(),
            Some("first".to_owned())
        );
    }

    #[test]
    fn store_replaces_the_previous_value() {
        let dir = repo();
        let store = Store::open(dir.path()).unwrap();
        store
            .store("refs/meta/bag", &"first".to_owned(), "write")
            .unwrap();
        store
            .store("refs/meta/bag", &"second".to_owned(), "write")
            .unwrap();
        assert_eq!(
            store.load::<String>("refs/meta/bag").unwrap(),
            Some("second".to_owned())
        );
    }

    #[test]
    fn store_tree_then_ref_tree_round_trips() {
        let dir = repo();
        let store = Store::open(dir.path()).unwrap();
        let tree = facet_git_tree::serialize_into(&"payload".to_owned(), &store.odb).unwrap();
        store.store_tree("refs/meta/raw", tree, "write").unwrap();
        assert_eq!(store.ref_tree("refs/meta/raw").unwrap(), tree);
    }

    #[test]
    fn ref_tree_errors_when_the_ref_is_absent() {
        let dir = repo();
        let store = Store::open(dir.path()).unwrap();
        let _ = store.ref_tree("refs/meta/missing").unwrap_err();
    }

    #[test]
    fn store_tree_advances_the_ref_and_keeps_the_new_tip() {
        let dir = repo();
        let store = Store::open(dir.path()).unwrap();
        let first = facet_git_tree::serialize_into(&"first".to_owned(), &store.odb).unwrap();
        let second = facet_git_tree::serialize_into(&"second".to_owned(), &store.odb).unwrap();
        store.store_tree("refs/meta/raw", first, "write").unwrap();
        store.store_tree("refs/meta/raw", second, "write").unwrap();
        assert_eq!(store.ref_tree("refs/meta/raw").unwrap(), second);
    }

    #[test]
    fn delete_ref_removes_the_ref() {
        let dir = repo();
        let store = Store::open(dir.path()).unwrap();
        let tree = facet_git_tree::serialize_into(&"payload".to_owned(), &store.odb).unwrap();
        store.store_tree("refs/meta/raw", tree, "write").unwrap();
        store.delete_ref("refs/meta/raw").unwrap();
        let _ = store.ref_tree("refs/meta/raw").unwrap_err();
    }

    #[test]
    fn store_tree_replace_writes_parentless_commits() {
        let dir = repo();
        let store = Store::open(dir.path()).unwrap();
        let first = facet_git_tree::serialize_into(&"first".to_owned(), &store.odb).unwrap();
        let second = facet_git_tree::serialize_into(&"second".to_owned(), &store.odb).unwrap();
        store
            .store_tree_replace("refs/meta/raw", first, "write")
            .unwrap();
        store
            .store_tree_replace("refs/meta/raw", second, "write")
            .unwrap();
        assert_eq!(store.ref_tree("refs/meta/raw").unwrap(), second);
        let tip = store.ref_commit("refs/meta/raw").unwrap().unwrap();
        let commit = store.read_commit(&tip).unwrap();
        assert!(commit.parents.is_empty());
    }

    /// A minimal keyed item, used to exercise `load_item`/`store_item`/
    /// `list_items`/`store_keyed`.
    #[derive(Facet, Clone, Debug, PartialEq)]
    struct Item {
        id: String,
        value: String,
    }

    impl HasId for Item {
        fn id(&self) -> &str {
            &self.id
        }
    }

    #[test]
    fn store_item_then_load_item_round_trips() {
        let dir = repo();
        let store = Store::open(dir.path()).unwrap();
        let item = Item {
            id: "a".into(),
            value: "1".into(),
        };
        store
            .store_item("refs/meta/items", "a", &item, "write")
            .unwrap();
        assert_eq!(
            store.load_item::<Item>("refs/meta/items", "a").unwrap(),
            Some(item)
        );
        assert_eq!(
            store
                .load_item::<Item>("refs/meta/items", "missing")
                .unwrap(),
            None
        );
    }

    #[test]
    fn store_keyed_uses_the_value_own_id() {
        let dir = repo();
        let store = Store::open(dir.path()).unwrap();
        let item = Item {
            id: "a".into(),
            value: "1".into(),
        };
        store
            .store_keyed("refs/meta/items", &item, "write")
            .unwrap();
        assert_eq!(
            store.load_item::<Item>("refs/meta/items", "a").unwrap(),
            Some(item)
        );
    }

    #[test]
    fn list_items_returns_every_item_keyed_by_id() {
        let dir = repo();
        let store = Store::open(dir.path()).unwrap();
        store
            .store_keyed(
                "refs/meta/items",
                &Item {
                    id: "a".into(),
                    value: "1".into(),
                },
                "write",
            )
            .unwrap();
        store
            .store_keyed(
                "refs/meta/items",
                &Item {
                    id: "b".into(),
                    value: "2".into(),
                },
                "write",
            )
            .unwrap();
        let mut ids: Vec<String> = store
            .list_items::<Item>("refs/meta/items")
            .unwrap()
            .into_iter()
            .map(|(id, _item)| id)
            .collect();
        ids.sort();
        assert_eq!(ids, vec!["a".to_owned(), "b".to_owned()]);
    }

    #[test]
    fn provenance_recovers_the_creating_and_updating_authors() {
        let dir = repo();
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.provenance("refs/meta/doc").unwrap(), None);
        store
            .store_authored(
                "refs/meta/doc",
                &"first".to_owned(),
                "write",
                ("alice", "alice@example.com"),
            )
            .unwrap();
        store
            .store_authored(
                "refs/meta/doc",
                &"second".to_owned(),
                "write",
                ("bob", "bob@example.com"),
            )
            .unwrap();
        let provenance = store.provenance("refs/meta/doc").unwrap().unwrap();
        assert_eq!(provenance.created.name, "alice");
        assert_eq!(provenance.created.email, "alice@example.com");
        assert_eq!(provenance.updated.name, "bob");
        assert_eq!(provenance.updated.email, "bob@example.com");
        assert!(provenance.created.seconds > 0);
    }

    /// A small multi-field, multi-collection document used to exercise the
    /// structural merge: two scalar fields plus a scalar-keyed map.
    #[derive(Facet, Clone, Debug, PartialEq)]
    struct Doc {
        name: String,
        note: String,
        tags: BTreeMap<String, String>,
    }

    // @relation(storage.concurrency, role=Verifies)
    #[test]
    fn merge_disjoint_struct_fields_combine() {
        let dir = repo();
        let store = Store::open(dir.path()).unwrap();
        let base = Doc {
            name: "a".into(),
            note: "x".into(),
            tags: BTreeMap::new(),
        };
        let ours = Doc {
            name: "b".into(),
            ..base.clone()
        };
        let theirs = Doc {
            note: "y".into(),
            ..base.clone()
        };
        let base_tree = facet_git_tree::serialize_into(&base, &store.odb).unwrap();
        let ours_tree = facet_git_tree::serialize_into(&ours, &store.odb).unwrap();
        let theirs_tree = facet_git_tree::serialize_into(&theirs, &store.odb).unwrap();

        let merged_tree =
            merge::three_way_merge::<Doc>(base_tree, ours_tree, theirs_tree, &store.odb).unwrap();
        let merged: Doc = facet_git_tree::deserialize(&merged_tree, &store.odb).unwrap();

        assert_eq!(
            merged,
            Doc {
                name: "b".into(),
                note: "y".into(),
                tags: BTreeMap::new(),
            }
        );
    }

    // @relation(storage.concurrency, role=Verifies)
    #[test]
    fn merge_disjoint_map_entries_combine() {
        let dir = repo();
        let store = Store::open(dir.path()).unwrap();
        let base = Doc {
            name: "a".into(),
            note: "x".into(),
            tags: BTreeMap::new(),
        };
        let ours = Doc {
            tags: entries(&[("x", "1")]),
            ..base.clone()
        };
        let theirs = Doc {
            tags: entries(&[("y", "2")]),
            ..base.clone()
        };
        let base_tree = facet_git_tree::serialize_into(&base, &store.odb).unwrap();
        let ours_tree = facet_git_tree::serialize_into(&ours, &store.odb).unwrap();
        let theirs_tree = facet_git_tree::serialize_into(&theirs, &store.odb).unwrap();

        let merged_tree =
            merge::three_way_merge::<Doc>(base_tree, ours_tree, theirs_tree, &store.odb).unwrap();
        let merged: Doc = facet_git_tree::deserialize(&merged_tree, &store.odb).unwrap();

        assert_eq!(merged.tags, entries(&[("x", "1"), ("y", "2")]));
    }

    // @relation(storage.concurrency, role=Verifies)
    #[test]
    fn merge_same_scalar_changed_both_ways_conflicts() {
        let dir = repo();
        let store = Store::open(dir.path()).unwrap();
        let base = Doc {
            name: "a".into(),
            note: "x".into(),
            tags: BTreeMap::new(),
        };
        let ours = Doc {
            name: "b".into(),
            ..base.clone()
        };
        let theirs = Doc {
            name: "c".into(),
            ..base.clone()
        };
        let base_tree = facet_git_tree::serialize_into(&base, &store.odb).unwrap();
        let ours_tree = facet_git_tree::serialize_into(&ours, &store.odb).unwrap();
        let theirs_tree = facet_git_tree::serialize_into(&theirs, &store.odb).unwrap();

        let result = merge::three_way_merge::<Doc>(base_tree, ours_tree, theirs_tree, &store.odb);
        assert!(matches!(result, Err(Error::Conflict)));
    }

    #[test]
    fn merge_map_key_removed_on_one_side_and_untouched_on_the_other_is_dropped() {
        let dir = repo();
        let store = Store::open(dir.path()).unwrap();
        let base = Doc {
            name: "a".into(),
            note: "x".into(),
            tags: entries(&[("x", "1")]),
        };
        let ours = Doc {
            tags: BTreeMap::new(), // we removed "x"
            ..base.clone()
        };
        let theirs = base.clone(); // untouched
        let base_tree = facet_git_tree::serialize_into(&base, &store.odb).unwrap();
        let ours_tree = facet_git_tree::serialize_into(&ours, &store.odb).unwrap();
        let theirs_tree = facet_git_tree::serialize_into(&theirs, &store.odb).unwrap();

        let merged_tree =
            merge::three_way_merge::<Doc>(base_tree, ours_tree, theirs_tree, &store.odb).unwrap();
        let merged: Doc = facet_git_tree::deserialize(&merged_tree, &store.odb).unwrap();

        assert!(merged.tags.is_empty());
    }

    #[test]
    fn merge_map_key_removed_on_one_side_and_modified_on_the_other_conflicts() {
        let dir = repo();
        let store = Store::open(dir.path()).unwrap();
        let base = Doc {
            name: "a".into(),
            note: "x".into(),
            tags: entries(&[("x", "1")]),
        };
        let ours = Doc {
            tags: BTreeMap::new(), // we removed "x"
            ..base.clone()
        };
        let theirs = Doc {
            tags: entries(&[("x", "2")]), // they changed "x"
            ..base.clone()
        };
        let base_tree = facet_git_tree::serialize_into(&base, &store.odb).unwrap();
        let ours_tree = facet_git_tree::serialize_into(&ours, &store.odb).unwrap();
        let theirs_tree = facet_git_tree::serialize_into(&theirs, &store.odb).unwrap();

        let result = merge::three_way_merge::<Doc>(base_tree, ours_tree, theirs_tree, &store.odb);
        assert!(matches!(result, Err(Error::Conflict)));
    }

    // @relation(storage.concurrency, role=Verifies)
    #[test]
    fn try_set_ref_conflicts_on_a_stale_expected() {
        let dir = repo();
        let store = Store::open(dir.path()).unwrap();
        let refname = "refs/meta/doc";
        store.store(refname, &"first".to_string(), "write").unwrap();
        let stale = store.ref_commit(refname).unwrap();

        // A second write lands, moving the ref past `stale`.
        store
            .store(refname, &"second".to_string(), "write")
            .unwrap();

        // A write built from the now-stale snapshot loses the CAS race.
        let tree = facet_git_tree::serialize_into(&"third".to_string(), &store.odb).unwrap();
        let commit = store
            .write_commit(tree, stale.into_iter().collect(), stale, "write", None)
            .unwrap();
        let result = store.try_set_ref(refname, stale, commit);
        assert!(matches!(result, Err(Error::Conflict)));
    }

    // @relation(storage.concurrency, role=Verifies)
    #[test]
    fn store_conflicts_on_a_fresh_ref_race_with_no_common_base() {
        let dir = repo();
        let store = Store::open(dir.path()).unwrap();
        let refname = "refs/meta/new-doc";

        // Someone else creates the ref first.
        store
            .store(refname, &"theirs".to_string(), "theirs")
            .unwrap();

        // Our write, built assuming the ref was still absent, has no common
        // ancestor with theirs and so cannot be merged.
        let tree = facet_git_tree::serialize_into(&"ours".to_string(), &store.odb).unwrap();
        let commit = store
            .write_commit(tree, Vec::new(), None, "ours", None)
            .unwrap();
        let result = store.try_set_ref(refname, None, commit);
        assert!(matches!(result, Err(Error::Conflict)));
    }

    // @relation(storage.concurrency, role=Verifies)
    #[test]
    fn amend_fails_closed_on_a_race_instead_of_merging() {
        let dir = repo();
        let store = Store::open(dir.path()).unwrap();
        let refname = "refs/meta/run";
        store
            .amend(refname, &"queued".to_string(), "queue")
            .unwrap();
        let stale = store.ref_commit(refname).unwrap();

        // A concurrent advance we never saw.
        store
            .amend(refname, &"running".to_string(), "advance to running")
            .unwrap();

        // Our own advance, built from the stale snapshot: same primitives
        // `amend` itself uses, so this exercises its exact CAS behavior.
        let parents = match stale {
            Some(tip) => store.read_commit(&tip).unwrap().parents,
            None => Vec::new(),
        };
        let tree = facet_git_tree::serialize_into(&"pass".to_string(), &store.odb).unwrap();
        let commit = store
            .write_commit(tree, parents, stale, "advance to pass", None)
            .unwrap();
        let result = store.try_set_ref(refname, stale, commit);
        assert!(matches!(result, Err(Error::Conflict)));
    }

    #[test]
    fn store_then_load_round_trips_through_the_schema_marker() {
        let dir = repo();
        let store = Store::open(dir.path()).unwrap();
        let item = Item {
            id: "a".into(),
            value: "1".into(),
        };
        store.store("refs/meta/doc", &item, "write").unwrap();

        // The written tree carries a `.schema` marker alongside the
        // document's own fields.
        let tree = store.ref_tree("refs/meta/doc").unwrap();
        let (_stripped, version) = schema::strip(&store.odb, tree).unwrap();
        assert_eq!(version, 1);

        assert_eq!(store.load::<Item>("refs/meta/doc").unwrap(), Some(item));
    }

    #[test]
    fn a_tree_with_no_schema_marker_reads_as_version_1() {
        let dir = repo();
        let store = Store::open(dir.path()).unwrap();
        let item = Item {
            id: "a".into(),
            value: "1".into(),
        };
        // Written directly through `facet_git_tree`, bypassing `Store::store`
        // and so its `.schema` injection — the on-disk shape of data written
        // before the marker existed.
        let tree = facet_git_tree::serialize_into(&item, &store.odb).unwrap();
        store.store_tree("refs/meta/legacy", tree, "write").unwrap();

        assert_eq!(store.load::<Item>("refs/meta/legacy").unwrap(), Some(item));
    }

    #[test]
    fn a_schema_version_newer_than_supported_errors_cleanly() {
        let dir = repo();
        let store = Store::open(dir.path()).unwrap();
        let item = Item {
            id: "a".into(),
            value: "1".into(),
        };
        let tree = facet_git_tree::serialize_into(&item, &store.odb).unwrap();
        let tree = schema::inject(&store.odb, tree, 2).unwrap();
        store.store_tree("refs/meta/future", tree, "write").unwrap();

        let error = store.load::<Item>("refs/meta/future").unwrap_err();
        assert!(matches!(
            error,
            Error::UnsupportedSchema {
                found: 2,
                supported: 1
            }
        ));
        assert_eq!(error.to_string(), "schema 2, this binary reads 1");
    }
}
