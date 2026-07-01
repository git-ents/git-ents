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

use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::path::Path;

use facet::Facet;
use gix::ObjectId;
use gix::objs::{Commit, FindExt as _, Write as _};
use gix::refs::Target;
use gix::refs::transaction::PreviousValue;

mod merge;

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
}

/// A meta-ref document that is a single named map of string keys to string
/// values — the shape the check set, the revocation list, and a run's outcomes
/// all share. The wrapping struct's one field fixes the on-disk subtree name
/// (`checks/`, `revoked/`, `results/`), so each document stays its own type;
/// this trait is only the bridge that lets them share the load/store plumbing
/// in [`Store::load_entries`] and [`Store::store_entries`].
pub trait MapDoc: for<'a> Facet<'a> {
    /// Wrap `entries` as the document.
    fn from_entries(entries: BTreeMap<String, String>) -> Self;
    /// The document's entries, consuming it.
    fn into_entries(self) -> BTreeMap<String, String>;
}

/// One `(key, value)` entry of a [`MapDoc`] presented as a named type. The set
/// documents expose legible structs (`Check`, `Revocation`, `RunOutcome`) rather
/// than bare pairs; this trait is the single bridge between such a struct and
/// the `(key, value)` shape stored on disk, so the wrap/unwrap is written once
/// here instead of at every load and store.
pub trait Row {
    /// Build a row from its stored `key` and `value`.
    fn from_pair(key: String, value: String) -> Self;
    /// The row's `(key, value)`, consuming it.
    fn into_pair(self) -> (String, String);
}

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
    /// Open the typed store for the repository at `repo`.
    pub fn open(repo: &Path) -> Result<Self, Error> {
        let repo = gix::open(repo).map_err(|error| Error::Open(Box::new(error)))?;
        let odb = gix::odb::at(repo.common_dir().join("objects")).map_err(|_io| Error::Odb)?;
        Ok(Self { repo, odb })
    }

    /// Load the document on `refname`, or `None` when the ref is absent.
    pub fn load<T: for<'a> Facet<'a>>(&self, refname: &str) -> Result<Option<T>, Error> {
        let Some(commit) = self.ref_commit(refname)? else {
            return Ok(None);
        };
        let tree = self.read_commit(&commit)?.tree;
        Ok(Some(facet_git_tree::deserialize(&tree, &self.odb)?))
    }

    /// Write `value` to `refname` as a new commit on top of the ref's current
    /// tip, so the update fast-forwards and accrues history.
    ///
    /// Uses compare-and-swap: if a concurrent writer moved the ref first,
    /// this attempts a schema-aware structural merge of the two documents
    /// against their common base and retries, up to [`MAX_MERGE_RETRIES`]
    /// times, before giving up with [`Error::Conflict`].
    pub fn store<T: for<'a> Facet<'a>>(
        &self,
        refname: &str,
        value: &T,
        message: &str,
    ) -> Result<(), Error> {
        self.store_impl(refname, value, message, None)
    }

    /// Like [`store`](Self::store), but attributing authorship to `author`
    /// (a `(name, email)` pair) while the committer stays the git-ents system
    /// identity — the way a web edit records the human who made the change while
    /// the server is the committer.
    pub fn store_authored<T: for<'a> Facet<'a>>(
        &self,
        refname: &str,
        value: &T,
        message: &str,
        author: (&str, &str),
    ) -> Result<(), Error> {
        self.store_impl(refname, value, message, Some(author))
    }

    fn store_impl<T: for<'a> Facet<'a>>(
        &self,
        refname: &str,
        value: &T,
        message: &str,
        author: Option<(&str, &str)>,
    ) -> Result<(), Error> {
        let mut expected = self.ref_commit(refname)?;
        let mut tree = facet_git_tree::serialize_into(value, &self.odb)?;
        for _ in 0..=MAX_MERGE_RETRIES {
            let parents = expected.into_iter().collect();
            let commit = self.write_commit(tree, parents, message, author)?;
            match self.try_set_ref(refname, expected, commit) {
                Ok(()) => return Ok(()),
                Err(Error::Conflict) => {
                    // No common ancestor (two independent geneses racing to
                    // create the same ref) can't be merged; fail closed.
                    let Some(base) = expected else {
                        return Err(Error::Conflict);
                    };
                    let theirs = self.ref_commit(refname)?.ok_or(Error::Conflict)?;
                    let base_tree = self.read_commit(&base)?.tree;
                    let theirs_tree = self.read_commit(&theirs)?.tree;
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
    pub fn amend<T: for<'a> Facet<'a>>(
        &self,
        refname: &str,
        value: &T,
        message: &str,
    ) -> Result<(), Error> {
        let tree = facet_git_tree::serialize_into(value, &self.odb)?;
        let expected = self.ref_commit(refname)?;
        let parents = match &expected {
            Some(tip) => self.read_commit(tip)?.parents,
            None => Vec::new(),
        };
        let commit = self.write_commit(tree, parents, message, None)?;
        self.try_set_ref(refname, expected, commit)
    }

    /// Load the [`MapDoc`] on `refname` as its `(key, value)` entries, or an
    /// empty vec when the ref is absent. Centralizes the "missing ref reads
    /// empty" policy the set documents share.
    pub fn load_entries<T: MapDoc>(&self, refname: &str) -> Result<Vec<(String, String)>, Error> {
        Ok(self
            .load::<T>(refname)?
            .map(|doc| doc.into_entries().into_iter().collect())
            .unwrap_or_default())
    }

    /// Store `entries` as the [`MapDoc`] `T` on `refname` as a new commit.
    pub fn store_entries<T: MapDoc>(
        &self,
        refname: &str,
        entries: BTreeMap<String, String>,
        message: &str,
    ) -> Result<(), Error> {
        self.store(refname, &T::from_entries(entries), message)
    }

    /// Load the [`MapDoc`] `D` on `refname` as its [`Row`] values `R`, or an
    /// empty vec when the ref is absent.
    pub fn load_rows<D: MapDoc, R: Row>(&self, refname: &str) -> Result<Vec<R>, Error> {
        Ok(self
            .load_entries::<D>(refname)?
            .into_iter()
            .map(|(key, value)| R::from_pair(key, value))
            .collect())
    }

    /// Store `rows` as the [`MapDoc`] `D` on `refname` as a new commit.
    pub fn store_rows<D: MapDoc, R: Row>(
        &self,
        refname: &str,
        rows: impl IntoIterator<Item = R>,
        message: &str,
    ) -> Result<(), Error> {
        self.store_entries::<D>(
            refname,
            rows.into_iter().map(Row::into_pair).collect(),
            message,
        )
    }

    /// The documents on `refname`'s commit chain as `(committer date, value)`
    /// pairs, newest first — one entry per commit, following first parents.
    pub fn history<T: for<'a> Facet<'a>>(&self, refname: &str) -> Result<Vec<(u64, T)>, Error> {
        let mut out = Vec::new();
        let mut cursor = self.ref_commit(refname)?;
        while let Some(oid) = cursor {
            let commit = self.read_commit(&oid)?;
            let value = facet_git_tree::deserialize(&commit.tree, &self.odb)?;
            out.push((commit.seconds, value));
            cursor = commit.parents.into_iter().next();
        }
        Ok(out)
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

    /// Read `oid`'s tree, parents, and committer date from the durable store.
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
        Ok(CommitFacts {
            tree: commit.tree(),
            parents: commit.parents().collect(),
            seconds: u64::try_from(seconds).unwrap_or(0),
        })
    }

    /// Wrap `tree` in a commit over `parents` and write it to the durable store.
    /// The committer is always the git-ents system identity; `author` overrides
    /// the authorship when set, otherwise it too is the system identity.
    fn write_commit(
        &self,
        tree: ObjectId,
        parents: Vec<ObjectId>,
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
        let commit = Commit {
            tree,
            parents: parents.into(),
            author,
            committer,
            encoding: None,
            message: message.into(),
            extra_headers: Vec::new(),
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

/// The facts read off a commit: its tree, its parents, and its committer date.
struct CommitFacts {
    tree: ObjectId,
    parents: Vec<ObjectId>,
    seconds: u64,
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::let_underscore_must_use,
        reason = "unit test"
    )]

    use std::process::Command;

    use super::*;

    /// A single-field map document, the shape [`MapDoc`] abstracts over.
    #[derive(Facet)]
    struct Bag {
        items: BTreeMap<String, String>,
    }

    impl MapDoc for Bag {
        fn from_entries(entries: BTreeMap<String, String>) -> Self {
            Self { items: entries }
        }

        fn into_entries(self) -> BTreeMap<String, String> {
            self.items
        }
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let status = Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["init", "-q"])
            .status()
            .unwrap();
        assert!(status.success());
        dir
    }

    fn entries(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn absent_ref_loads_no_entries() {
        let dir = repo();
        let store = Store::open(dir.path()).unwrap();
        assert!(
            store
                .load_entries::<Bag>("refs/meta/bag")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn store_entries_round_trips() {
        let dir = repo();
        let store = Store::open(dir.path()).unwrap();
        let written = entries(&[("a", "1"), ("b", "2")]);
        store
            .store_entries::<Bag>("refs/meta/bag", written.clone(), "write")
            .unwrap();
        let loaded: BTreeMap<String, String> = store
            .load_entries::<Bag>("refs/meta/bag")
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(loaded, written);
    }

    #[test]
    fn store_entries_replaces_the_previous_set() {
        let dir = repo();
        let store = Store::open(dir.path()).unwrap();
        store
            .store_entries::<Bag>("refs/meta/bag", entries(&[("a", "1")]), "write")
            .unwrap();
        store
            .store_entries::<Bag>("refs/meta/bag", entries(&[("b", "2")]), "write")
            .unwrap();
        let loaded: BTreeMap<String, String> = store
            .load_entries::<Bag>("refs/meta/bag")
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(loaded, entries(&[("b", "2")]));
    }

    /// A small multi-field, multi-collection document used to exercise the
    /// structural merge: two scalar fields plus a scalar-keyed map.
    #[derive(Facet, Clone, Debug, PartialEq)]
    struct Doc {
        name: String,
        note: String,
        tags: BTreeMap<String, String>,
    }

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
            .write_commit(tree, stale.into_iter().collect(), "write", None)
            .unwrap();
        let result = store.try_set_ref(refname, stale, commit);
        assert!(matches!(result, Err(Error::Conflict)));
    }

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
        let commit = store.write_commit(tree, Vec::new(), "ours", None).unwrap();
        let result = store.try_set_ref(refname, None, commit);
        assert!(matches!(result, Err(Error::Conflict)));
    }

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
            .write_commit(tree, parents, "advance to pass", None)
            .unwrap();
        let result = store.try_set_ref(refname, stale, commit);
        assert!(matches!(result, Err(Error::Conflict)));
    }
}
