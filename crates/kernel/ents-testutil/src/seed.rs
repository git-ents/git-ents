//! Seeding helpers: members, meta entities, results, and code-ref history.

use ents_model::{Member, MemberId, Provenance, ResultRecord, Status, namespace};
use gix::refs::FullName;
use gix_hash::ObjectId;
use gix_object::{Find, Kind, Write};

use crate::commit::{CommitSpec, write_commit};
use crate::keys::Keypair;
use crate::refs::MemRefStore;

/// Write the empty tree into `objects` and return its id.
///
/// # Examples
///
/// ```
/// use ents_testutil::{ObjectStore, empty_tree};
///
/// let objects = ObjectStore::default();
/// let tree = empty_tree(&objects);
/// assert_eq!(tree.to_string(), "4b825dc642cb6eb9a060e54bf8d69288fbee4904");
/// ```
pub fn empty_tree(objects: &impl Write) -> ObjectId {
    objects
        .write_buf(Kind::Tree, b"")
        .expect("in-memory object write cannot fail")
}

/// Serialize `entity` as its typed tree and land it on `refname` as a
/// mutation commit signed by `signer` when one is given. Parents come from
/// `refname`'s current tip. Returns the new tip. The refname is bound to
/// the signed content by the gate (`meta-ref.identity-binding`), not by
/// any commit trailer.
///
/// # Examples
///
/// ```
/// use ents_testutil::{Keypair, MemRefStore, ObjectStore, write_meta_entity};
/// use gix_ref_store::RefStoreRead;
///
/// let refs = MemRefStore::default();
/// let objects = ObjectStore::default();
/// let name: gix::refs::FullName = "refs/meta/redactions/1".try_into().expect("valid");
///
/// let redaction = ents_model::Redaction::new(
///     gix_hash::ObjectId::null(gix_hash::Kind::Sha1),
///     "leaked credential",
/// );
/// let tip = write_meta_entity(&refs, &objects, name.clone(), &redaction, None, 1_000);
/// assert_eq!(refs.get(name.as_ref()).expect("readable"), Some(tip));
/// ```
pub fn write_meta_entity<T: for<'facet> facet::Facet<'facet>>(
    refs: &MemRefStore,
    objects: &(impl Write + Find),
    refname: FullName,
    entity: &T,
    signer: Option<&Keypair>,
    seconds: i64,
) -> ObjectId {
    let tree =
        facet_git_tree::serialize_into(entity, objects).expect("fixture entity always serializes");
    let message = format!("Mutate {}", refname.as_bstr());
    let parents = crate::refs_get(refs, &refname).into_iter().collect();
    let tip = write_commit(
        objects,
        &CommitSpec {
            tree,
            parents,
            message,
            seconds,
        },
        signer,
    );
    refs.set(refname.as_ref(), tip);
    tip
}

/// Enroll `id` as a member whose key is `key`'s public half, active, with
/// the given provenance, and return the member ref's new tip.
///
/// The enrollment commit is signed by the member's own key — the
/// self-admitting shape the bootstrap window admits.
///
/// # Examples
///
/// ```
/// use ents_model::Provenance;
/// use ents_testutil::{Keypair, MemRefStore, ObjectStore, enroll_member};
///
/// let refs = MemRefStore::default();
/// let objects = ObjectStore::default();
/// enroll_member(&refs, &objects, "jdc", &Keypair::from_seed(1), Provenance::AdminRegistered, 500);
/// ```
pub fn enroll_member(
    refs: &MemRefStore,
    objects: &(impl Write + Find),
    id: &str,
    key: &Keypair,
    provenance: Provenance,
    seconds: i64,
) -> ObjectId {
    let member = Member::new(id, key.public_openssh(), provenance);
    write_member(refs, objects, id, &member, Some(key), seconds)
}

/// Land an arbitrary [`Member`] state on `refs/meta/member/<id>` — the
/// general form of [`enroll_member`], for revocations, unrevocations, and
/// promotions, signed by whoever `signer` is (an admin, usually).
///
/// # Examples
///
/// ```
/// use ents_model::{Member, Provenance};
/// use ents_testutil::{Keypair, MemRefStore, ObjectStore, write_member};
///
/// let refs = MemRefStore::default();
/// let objects = ObjectStore::default();
/// let key = Keypair::from_seed(1);
///
/// let mut member = Member::new("jdc", key.public_openssh(), Provenance::AdminRegistered);
/// member.revoke();
/// write_member(&refs, &objects, "jdc", &member, Some(&key), 900);
/// ```
pub fn write_member(
    refs: &MemRefStore,
    objects: &(impl Write + Find),
    id: &str,
    member: &Member,
    signer: Option<&Keypair>,
    seconds: i64,
) -> ObjectId {
    let refname = namespace::member_ref(&MemberId::new(id)).expect("valid member id in fixture");
    write_meta_entity(refs, objects, refname, member, signer, seconds)
}

/// Record a result of `status` for `effect` on the commit whose hex id
/// starts with `short_oid`, at the canonical
/// `refs/meta/results/<effect>/<short_oid>` ref (`effect.results-writeback`).
///
/// The tree is a [`ResultRecord`] carrying `effect` and a target oid whose
/// hex begins with `short_oid` (`model.result-identity`): when `short_oid`
/// is a hex prefix it is right-padded with zeros to a full oid, so the
/// gate's identity binding recomputes the ref; when it is not hex, a null
/// target is used, sufficient for query scan tests that never gate.
///
/// The result commit is unsigned unless `signer` is given — query tests
/// exercise scan semantics, gate tests exercise signatures.
///
/// # Examples
///
/// ```
/// use ents_model::Status;
/// use ents_testutil::{MemRefStore, ObjectStore, record_result};
///
/// let refs = MemRefStore::default();
/// let objects = ObjectStore::default();
/// record_result(&refs, &objects, "unit", "abc123", Status::Pass, None, 1_000);
/// ```
pub fn record_result(
    refs: &MemRefStore,
    objects: &(impl Write + Find),
    effect: &str,
    short_oid: &str,
    status: Status,
    signer: Option<&Keypair>,
    seconds: i64,
) -> ObjectId {
    let refname =
        namespace::result_ref(effect, short_oid).expect("valid result segments in fixture");
    let target = target_for(short_oid);
    let record = ResultRecord::new(effect, target, status);
    write_meta_entity(refs, objects, refname, &record, signer, seconds)
}

/// An oid whose hex form begins with `short_oid`: right-pad a hex prefix
/// with zeros to 40 chars, or fall back to the null oid when `short_oid`
/// is not hex (query scan fixtures do not gate on the target).
fn target_for(short_oid: &str) -> ObjectId {
    let padded = format!("{short_oid:0<40}");
    ObjectId::from_hex(padded.as_bytes())
        .unwrap_or_else(|_| ObjectId::null(gix_hash::Kind::Sha1))
}

/// Append `count` empty-tree commits on top of `refname`'s current tip
/// (creating the ref if absent), one second apart starting at
/// `start_seconds`, and return the new commits oldest-first.
///
/// # Examples
///
/// ```
/// use ents_testutil::{MemRefStore, ObjectStore, advance_ref};
/// use gix_ref_store::RefStoreRead;
///
/// let refs = MemRefStore::default();
/// let objects = ObjectStore::default();
/// let commits = advance_ref(&refs, &objects, "refs/heads/main", 3, 100);
/// assert_eq!(commits.len(), 3);
///
/// let name: gix::refs::FullName = "refs/heads/main".try_into().expect("valid");
/// assert_eq!(refs.get(name.as_ref()).expect("readable"), commits.last().copied());
/// ```
pub fn advance_ref(
    refs: &MemRefStore,
    objects: &(impl Write + Find),
    refname: &str,
    count: usize,
    start_seconds: i64,
) -> Vec<ObjectId> {
    let name: FullName = refname.try_into().expect("valid refname in fixture");
    let tree = empty_tree(objects);
    let mut tip = crate::refs_get(refs, &name);
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let seconds = start_seconds.saturating_add(i64::try_from(i).unwrap_or(i64::MAX));
        let commit = write_commit(
            objects,
            &CommitSpec {
                tree,
                parents: tip.into_iter().collect(),
                message: format!("{refname} commit {i} at {seconds}"),
                seconds,
            },
            None,
        );
        tip = Some(commit);
        out.push(commit);
    }
    if let Some(tip) = tip {
        refs.set(name.as_ref(), tip);
    }
    out
}
