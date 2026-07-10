//! A commit builder that signs the way git's SSH signing does.

use gix_hash::ObjectId;
use gix_object::{Commit, Kind, Write, WriteTo as _};

use crate::keys::Keypair;

/// The inputs for one fixture commit; see [`write_commit`].
///
/// Author and committer are fixed fixture identities — only the timestamp
/// varies, because timestamps are what revocation-boundary logic keys on.
///
/// # Examples
///
/// ```
/// use ents_testutil::{CommitSpec, ObjectStore, empty_tree, write_commit};
///
/// let objects = ObjectStore::default();
/// let tree = empty_tree(&objects);
/// let spec = CommitSpec {
///     tree,
///     parents: vec![],
///     message: "Initial".into(),
///     seconds: 1_000,
/// };
/// let oid = write_commit(&objects, &spec, None);
/// assert!(objects.get(&oid).is_some());
/// ```
#[derive(Debug, Clone)]
pub struct CommitSpec {
    /// The tree this commit records.
    pub tree: ObjectId,
    /// Parent commits, in order; empty for a root commit.
    pub parents: Vec<ObjectId>,
    /// The full commit message, trailers included.
    pub message: String,
    /// Author and committer timestamp, in seconds since the Unix epoch.
    pub seconds: i64,
}

fn actor(seconds: i64) -> gix::actor::Signature {
    gix::actor::Signature {
        name: "Fixture".into(),
        email: "fixture@ents.test".into(),
        time: gix::date::Time { seconds, offset: 0 },
    }
}

/// Write the commit described by `spec` into `objects`, signing it with
/// `key` when one is given.
///
/// Signing works exactly the way `git commit -S` with an SSH key does: the
/// SSHSIG (namespace `git`) is computed over the commit object serialized
/// *without* its `gpgsig` header, then stored as that header's value — so
/// the signature replicates with the repository and verifies offline in
/// every clone.
///
/// # Examples
///
/// ```
/// use ents_testutil::{CommitSpec, Keypair, ObjectStore, empty_tree, write_commit};
///
/// let objects = ObjectStore::default();
/// let key = Keypair::from_seed(1);
/// let spec = CommitSpec {
///     tree: empty_tree(&objects),
///     parents: vec![],
///     message: "Signed".into(),
///     seconds: 1_000,
/// };
/// let oid = write_commit(&objects, &spec, Some(&key));
///
/// // The stored bytes carry the signature as repository data.
/// let raw = objects.get(&oid).expect("stored");
/// # let gix_object::Object::Commit(commit) = raw else { panic!("not a commit") };
/// assert!(commit.extra_headers.iter().any(|(k, _)| k == "gpgsig"));
/// ```
pub fn write_commit(objects: &impl Write, spec: &CommitSpec, key: Option<&Keypair>) -> ObjectId {
    let mut commit = Commit {
        tree: spec.tree,
        parents: spec.parents.clone().into(),
        author: actor(spec.seconds),
        committer: actor(spec.seconds),
        encoding: None,
        message: spec.message.clone().into(),
        extra_headers: Vec::new(),
    };

    if let Some(key) = key {
        let mut payload = Vec::new();
        commit
            .write_to(&mut payload)
            .expect("serializing a commit to a Vec cannot fail");
        let pem = key.sign(&payload);
        commit
            .extra_headers
            .push(("gpgsig".into(), pem.trim_end().into()));
    }

    let mut raw = Vec::new();
    commit
        .write_to(&mut raw)
        .expect("serializing a commit to a Vec cannot fail");
    objects
        .write_buf(Kind::Commit, &raw)
        .expect("in-memory object write cannot fail")
}
