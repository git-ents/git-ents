//! Typed documents on `refs/meta/*` refs, stored as git object graphs.
//!
//! A [`Store`] reads and writes [`Facet`] values to a ref's tree through
//! [`facet_git_tree`]: the value becomes a git tree, the tree is wrapped in a
//! commit parented on the ref's prior tip, and the ref is moved to it. The
//! commit chain is the document's history and each commit's date is its
//! timestamp, so nothing about versioning has to be modeled in the tree
//! itself. This is the single home for the plumbing that the signer set, the
//! check set, and the run log all share.

use std::cmp::Reverse;
use std::path::Path;

use facet::Facet;
use gix::ObjectId;
use gix::objs::{Commit, FindExt as _, Write as _};
use gix::refs::transaction::PreviousValue;

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
    pub fn store<T: for<'a> Facet<'a>>(
        &self,
        refname: &str,
        value: &T,
        message: &str,
    ) -> Result<(), Error> {
        let tree = facet_git_tree::serialize_into(value, &self.odb)?;
        let parents = self.ref_commit(refname)?.into_iter().collect();
        let commit = self.write_commit(tree, parents, message)?;
        self.set_ref(refname, commit)
    }

    /// Write `value` to `refname` in place, replacing the ref's tip commit
    /// (re-parented on the tip's own parents) rather than appending. Lets a
    /// single document advance through intermediate states without a commit per
    /// transition. When the ref is absent this starts a fresh history.
    pub fn amend<T: for<'a> Facet<'a>>(
        &self,
        refname: &str,
        value: &T,
        message: &str,
    ) -> Result<(), Error> {
        let tree = facet_git_tree::serialize_into(value, &self.odb)?;
        let parents = match self.ref_commit(refname)? {
            Some(tip) => self.read_commit(&tip)?.parents,
            None => Vec::new(),
        };
        let commit = self.write_commit(tree, parents, message)?;
        self.set_ref(refname, commit)
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
    fn write_commit(
        &self,
        tree: ObjectId,
        parents: Vec<ObjectId>,
        message: &str,
    ) -> Result<ObjectId, Error> {
        let signature = gix::actor::Signature {
            name: IDENTITY_NAME.into(),
            email: IDENTITY_EMAIL.into(),
            time: gix::date::Time::now_utc(),
        };
        let commit = Commit {
            tree,
            parents: parents.into(),
            author: signature.clone(),
            committer: signature,
            encoding: None,
            message: message.into(),
            extra_headers: Vec::new(),
        };
        self.odb
            .write(&commit)
            .map_err(|error| Error::Object(error.to_string()))
    }

    /// Point `refname` at `commit`, creating or force-updating it.
    fn set_ref(&self, refname: &str, commit: ObjectId) -> Result<(), Error> {
        self.repo
            .reference(refname, commit, PreviousValue::Any, "git-ents: update")
            .map_err(|error| Error::Ref(error.to_string()))?;
        Ok(())
    }
}

/// The facts read off a commit: its tree, its parents, and its committer date.
struct CommitFacts {
    tree: ObjectId,
    parents: Vec<ObjectId>,
    seconds: u64,
}
