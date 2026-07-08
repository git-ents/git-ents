//! Replay harness for [`git_protocol::CorpusEntry`] (`docs/scale-out.adoc`,
//! WS0's "op replay corpus" — the conformance seed corpus WS2 replays):
//! [`replay_corpus`] applies a logged corpus, in order, against any
//! `RefStore`+`ObjectStore` pair; [`reachable_object_set`] is the
//! "identical reachable object sets" half of the assertion a replay test
//! makes (the "identical final refs" half is an ordinary
//! `RefStore::iter_prefix` comparison, needing no helper here).

use std::collections::BTreeSet;

use git_backend::{Expected, ObjectStore, PackStream, RefEdit, RefStore, TxOutcome};
use git_protocol::CorpusEntry;
use gix_hash::ObjectId;

/// Replay `entries`, in order, against `refs`/`objects`: stage each
/// entry's pack, apply its ref edits as one atomic transaction (the same
/// shape the entry was originally accepted with), then promote once that
/// transaction applies — never before, so a replayed-but-rejected entry's
/// pack stays quarantined rather than becoming visible garbage.
///
/// A corpus is expected to replay cleanly against a backend starting from
/// the same state (typically empty) it was recorded from; an entry whose
/// recorded `old`/`new` no longer apply against `refs`'s current state is
/// reported as an error rather than silently skipped, since that means the
/// corpus and the target have already diverged — exactly what this
/// harness exists to catch.
///
/// # Errors
///
/// Returns an error if staging, transacting, or promoting any entry
/// fails, including a rejected compare-and-swap.
pub fn replay_corpus(
    entries: &[CorpusEntry],
    refs: &dyn RefStore,
    objects: &dyn ObjectStore,
) -> git_backend::Result<()> {
    for entry in entries {
        let quarantine =
            objects.stage_pack(PackStream::new(std::io::Cursor::new(entry.pack.clone())))?;
        let edits: Vec<RefEdit> = entry
            .ref_edits
            .iter()
            .map(|edit| RefEdit {
                name: edit.name.clone(),
                expected: match edit.old {
                    Some(oid) => Expected::MustExistAndMatch(oid),
                    None => Expected::MustNotExist,
                },
                new: edit.new,
            })
            .collect();
        match refs.transaction(&edits)? {
            TxOutcome::Applied => objects.promote(quarantine)?,
            TxOutcome::Rejected { name } => {
                return Err(git_backend::Error::RefStore(format!(
                    "corpus replay: ref {name} did not match its recorded expected value"
                )));
            }
        }
    }
    Ok(())
}

/// The set of every object reachable from `refs`' current tips over
/// `objects` — a thin wrapper over [`gix_reachability::gc_mark`] (no
/// reachability artifacts, so a plain walk) for a replay test to compare
/// between the original backend and the one it replayed a corpus into.
///
/// # Errors
///
/// Returns an error if the ref or object store cannot be read, or the walk
/// finds a ref tip whose history is incomplete.
pub fn reachable_object_set(
    refs: &dyn RefStore,
    objects: &dyn ObjectStore,
) -> gix_reachability::Result<BTreeSet<ObjectId>> {
    gix_reachability::gc_mark(refs, objects, &gix_reachability::ArtifactBundle::empty())
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "test fixture, not application code"
    )]

    use std::process::{Command, Stdio};

    use git_backend::RefName;
    use git_protocol::types::AppliedRefEdit;
    use git_store::test_support::{commit_all, head, repo};

    use super::*;

    /// Pack every object reachable from `commit` and not from `boundary`
    /// (or the commit's whole history, when `boundary` is `None`) — the
    /// same shape `git_hydrate::pre_receive::build_pack` produces for a
    /// real push, so this synthesized corpus exercises `replay_corpus`
    /// against realistic incremental packs, not just whole-history ones.
    fn pack_for(dir: &std::path::Path, commit: &str, boundary: Option<&str>) -> Vec<u8> {
        let mut rev_list_args = vec!["rev-list", "--objects", commit];
        if let Some(boundary) = boundary {
            rev_list_args.push("--not");
            rev_list_args.push(boundary);
        }
        let mut rev_list = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(&rev_list_args)
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn git rev-list");
        let pack_objects = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["pack-objects", "--stdout", "-q"])
            .stdin(rev_list.stdout.take().expect("rev-list stdout"))
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn git pack-objects");
        let output = pack_objects
            .wait_with_output()
            .expect("wait for pack-objects");
        assert!(rev_list.wait().expect("wait for rev-list").success());
        assert!(output.status.success());
        output.stdout
    }

    // @relation(role=Verifies)
    #[test]
    fn replays_a_synthesized_corpus_identically() {
        let dir = repo();
        std::fs::write(dir.path().join("file"), "one").expect("write fixture file");
        commit_all(dir.path(), "first");
        let commit1 = head(dir.path());
        std::fs::write(dir.path().join("file"), "two").expect("write fixture file");
        commit_all(dir.path(), "second");
        let commit2 = head(dir.path());

        let commit1_oid = ObjectId::from_hex(commit1.as_bytes()).expect("valid oid");
        let commit2_oid = ObjectId::from_hex(commit2.as_bytes()).expect("valid oid");

        let entries = vec![
            CorpusEntry::new(
                None,
                vec![AppliedRefEdit {
                    name: RefName::new("refs/heads/main"),
                    old: None,
                    new: Some(commit1_oid),
                }],
                pack_for(dir.path(), &commit1, None),
            ),
            CorpusEntry::new(
                None,
                vec![AppliedRefEdit {
                    name: RefName::new("refs/heads/main"),
                    old: Some(commit1_oid),
                    new: Some(commit2_oid),
                }],
                pack_for(dir.path(), &commit2, Some(&commit1)),
            ),
        ];

        let target = tempfile::tempdir().expect("tempdir");
        let status = Command::new("git")
            .arg("init")
            .arg("-q")
            .arg("--bare")
            .arg(target.path())
            .status()
            .expect("git init --bare");
        assert!(status.success());
        let refs = refstore_files::FilesRefStore::open(target.path()).expect("open refs");
        let objects = odb_files::OdbFiles::open(target.path()).expect("open objects");

        replay_corpus(&entries, &refs, &objects).expect("replay_corpus");

        let main = RefName::new("refs/heads/main");
        assert_eq!(refs.get(&main).expect("get"), Some(commit2_oid));

        let reachable = reachable_object_set(&refs, &objects).expect("reachable_object_set");
        assert!(reachable.contains(&commit1_oid));
        assert!(reachable.contains(&commit2_oid));
    }
}
