//! [`IngestPack`] on the storage traits: the correctness-critical trait, per
//! `docs/scale-out.adoc`'s correctness rules and "Attested push" section.
//!
//! Ordering, matching [`crate::IngestPack`]'s contract exactly:
//!
//! 1. attestation ([`crate::attestation::verify`]) — before anything is
//!    staged;
//! 2. the incoming pack is staged into quarantine
//!    ([`git_backend::ObjectStore::stage_pack`]);
//! 3. connectivity is checked against the union of that quarantine and the
//!    promoted store ([`crate::walk`]);
//! 4. one atomic [`git_backend::RefStore::transaction`] commits the
//!    caller's ref edits *and* the op record's ref
//!    ([`crate::attestation::OP_LOG_REF`]) together — the only commit
//!    point;
//! 5. only once that transaction applies are both quarantines promoted.
//!
//! A push that fails attestation or connectivity is rejected before step 2's
//! staged objects are ever promoted — they simply become unreferenced
//! garbage in quarantine, per causal collection safety (correctness rule 1):
//! nothing was ever committed to reach them.

use std::io::Read as _;

use git_backend::{Expected, PackStream, RefEdit, RefName, TxOutcome};
use gix_hash::ObjectId;
use gix_object::Kind;

use git_reachability::engine::accelerated_reachable;

use super::{BackendResolver, NativeBackend};
use crate::attestation::{self, OP_LOG_REF};
use crate::pack::{PackObject, build_pack};
use crate::types::{AppliedRefEdit, PushOutcome, PushRequest};
use crate::walk::ObjectSource;
use crate::{Error, IngestPack, Result};

/// An [`ObjectSource`] over the incoming pack's own scratch bundle (staged
/// only for this connectivity check, distinct from the real quarantine
/// `stage_pack` creates below) unioned with the repository's promoted
/// object store — exactly what a push's connectivity must resolve against:
/// objects it brings, plus objects it already has.
struct IncomingPackSource<'a> {
    bundle: Option<&'a gix_pack::Bundle>,
    store: &'a dyn git_backend::ObjectStore,
}

impl ObjectSource for IncomingPackSource<'_> {
    fn find(&self, id: &ObjectId) -> git_reachability::Result<Option<(Kind, Vec<u8>)>> {
        if let Some(bundle) = self.bundle {
            let mut buf = Vec::new();
            let mut inflate = gix_features::zlib::Inflate::default();
            let mut cache = gix_pack::cache::Never;
            if let Some((data, _location)) = bundle
                .find(id, &mut buf, &mut inflate, &mut cache)
                .map_err(|error| git_reachability::Error::Decode(error.to_string()))?
            {
                return Ok(Some((data.kind, data.data.to_vec())));
            }
        }
        if self.store.contains(*id)? {
            let object = self.store.read(*id)?;
            return Ok(Some((object.kind, object.data)));
        }
        Ok(None)
    }
}

impl<R: BackendResolver> IngestPack for NativeBackend<R> {
    fn receive(&self, push: PushRequest) -> Result<PushOutcome> {
        let backends = self.backends(&push.repo)?;

        let ref_names: Vec<&str> = push
            .ref_edits
            .iter()
            .map(|edit| edit.name.as_str())
            .collect();
        match attestation::verify(
            backends.authorized_members.clone(),
            &backends.config,
            push.push_cert.as_ref(),
            &ref_names,
        ) {
            Ok(_signer) => {}
            Err(Error::Attestation(reason)) => return Ok(PushOutcome::Rejected { reason }),
            Err(other) => return Err(other),
        }

        let PushRequest {
            repo: _,
            ref_edits,
            mut pack,
            push_cert,
        } = push;

        let mut pack_bytes = Vec::new();
        pack.read_to_end(&mut pack_bytes)?;

        // A scratch bundle purely for the connectivity walk below — not the
        // real quarantine, which `stage_pack` (the backend's own mechanism)
        // creates further down.
        let scratch = tempfile::tempdir()?;
        let mut reader = std::io::Cursor::new(&pack_bytes);
        let write_outcome = gix_pack::Bundle::write_to_directory(
            &mut reader,
            Some(scratch.path()),
            &mut gix::progress::Discard,
            &std::sync::atomic::AtomicBool::new(false),
            None::<gix::odb::Handle>,
            gix_pack::bundle::write::Options {
                object_hash: gix_hash::Kind::Sha1,
                ..Default::default()
            },
        )
        .map_err(|error| Error::Pack(error.to_string()))?;
        // An empty pack (a pure ref deletion, say) writes no index at all —
        // nothing in it to union with the promoted store either.
        let bundle = write_outcome
            .index_path
            .map(|index_path| {
                gix_pack::Bundle::at(index_path, gix_hash::Kind::Sha1)
                    .map_err(|error| Error::Pack(error.to_string()))
            })
            .transpose()?;
        let source = IncomingPackSource {
            bundle: bundle.as_ref(),
            store: backends.objects.as_ref(),
        };

        let roots: Vec<ObjectId> = ref_edits.iter().filter_map(|edit| edit.new).collect();
        let connectivity = accelerated_reachable(
            roots,
            &source,
            |id| backends.objects.contains(*id).unwrap_or(false),
            false,
            &backends.reachability,
        );
        match connectivity {
            Ok(_reachable) => {}
            Err(git_reachability::Error::MissingObject(id)) => {
                return Ok(PushOutcome::Rejected {
                    reason: format!("connectivity check failed: missing object {id}"),
                });
            }
            Err(other) => return Err(other.into()),
        }

        let quarantine = backends
            .objects
            .stage_pack(PackStream::new(std::io::Cursor::new(pack_bytes)))?;

        let cert_bytes = push_cert
            .as_ref()
            .map(|cert| cert.raw.as_bytes().to_vec())
            .unwrap_or_default();
        let cert_oid = gix_object::compute_hash(gix_hash::Kind::Sha1, Kind::Blob, &cert_bytes)
            .map_err(|error| Error::Pack(error.to_string()))?;

        let applied: Vec<AppliedRefEdit> = ref_edits
            .iter()
            .map(|edit| AppliedRefEdit {
                name: edit.name.clone(),
                old: match &edit.expected {
                    Expected::MustExistAndMatch(oid) => Some(*oid),
                    Expected::MustNotExist => None,
                    Expected::Any => backends.refs.get(&edit.name).ok().flatten(),
                },
                new: edit.new,
            })
            .collect();

        let prev = attestation::op_log_tip(backends.refs.as_ref())?;
        let (op_oid, mut op_objects) = attestation::build_op_record(
            prev,
            cert_oid,
            &applied,
            self.signer.as_ref(),
            backends.objects.as_ref(),
        )?;
        if !backends.objects.contains(cert_oid)? {
            op_objects.push(PackObject {
                id: cert_oid,
                kind: Kind::Blob,
                data: cert_bytes,
            });
        }
        let op_pack = build_pack(&op_objects)?;
        let op_quarantine = backends
            .objects
            .stage_pack(PackStream::new(std::io::Cursor::new(op_pack)))?;

        let mut edits = ref_edits;
        edits.push(RefEdit {
            name: RefName::new(OP_LOG_REF),
            expected: match prev {
                Some(oid) => Expected::MustExistAndMatch(oid),
                None => Expected::MustNotExist,
            },
            new: Some(op_oid),
        });

        match backends.refs.transaction(&edits)? {
            TxOutcome::Applied => {
                backends.objects.promote(quarantine)?;
                backends.objects.promote(op_quarantine)?;
                Ok(PushOutcome::Accepted {
                    push_id: op_oid,
                    applied,
                })
            }
            TxOutcome::Rejected { name } => Ok(PushOutcome::Rejected {
                reason: format!("ref {name} did not match its expected value; push rejected"),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "test fixture")]

    use git_backend::{Expected, RefEdit};
    use git_member::members::{Member, Provenance, Trust};

    use super::*;
    use crate::native::NativeBackend;
    use crate::native::test_support::{FixedResolver, bare_repo, commit_onto, test_signer};
    use crate::types::{PushCertificate, PushRequest};

    /// Build a real pack for `commit` (and everything it reaches) by
    /// shelling out to `git rev-list`/`git pack-objects` against `dir` — the
    /// same mechanism a real push transmits, mirroring `odb-files`'s own
    /// test helper.
    fn pack_for(dir: &std::path::Path, commit: ObjectId) -> Vec<u8> {
        let hex = commit.to_hex().to_string();
        let mut rev_list = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["rev-list", "--objects", &hex])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let pack_objects = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["pack-objects", "--stdout", "-q"])
            .stdin(rev_list.stdout.take().unwrap())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let output = pack_objects.wait_with_output().unwrap();
        assert!(rev_list.wait().unwrap().success());
        assert!(output.status.success());
        output.stdout
    }

    fn empty_pack() -> Vec<u8> {
        build_pack(&[]).unwrap()
    }

    #[test]
    fn accepts_a_push_during_the_bootstrap_window_and_emits_an_op_record() {
        let source = bare_repo();
        let commit = commit_onto(source.path(), "file", "content");
        let pack = pack_for(source.path(), commit);

        let dest = bare_repo();
        let (_key_dir, signer) = test_signer();
        let backend = NativeBackend::new(FixedResolver::open(dest.path()), signer);

        let push = PushRequest {
            repo: crate::RepoId::new("repo"),
            ref_edits: vec![RefEdit {
                name: RefName::new("refs/heads/main"),
                expected: Expected::MustNotExist,
                new: Some(commit),
            }],
            pack: PackStream::new(std::io::Cursor::new(pack)),
            push_cert: None,
        };
        let outcome = backend.receive(push).unwrap();
        assert!(
            matches!(outcome, PushOutcome::Accepted { .. }),
            "{outcome:?}"
        );
        if let PushOutcome::Accepted { push_id, applied } = outcome {
            assert_eq!(applied.len(), 1);
            assert_eq!(applied.first().and_then(|edit| edit.new), Some(commit));

            let objects = odb_files::OdbFiles::open(dest.path()).unwrap();
            assert!(git_backend::ObjectStore::contains(&objects, commit).unwrap());
            assert!(git_backend::ObjectStore::contains(&objects, push_id).unwrap());

            let refs = refstore_files::FilesRefStore::open(dest.path()).unwrap();
            assert_eq!(
                git_backend::RefStore::get(&refs, &RefName::new(crate::attestation::OP_LOG_REF))
                    .unwrap(),
                Some(push_id)
            );
        }
    }

    #[test]
    fn rejects_an_unsigned_push_once_a_member_is_enrolled() {
        let dest = bare_repo();
        let (_key_dir, signer) = test_signer();
        let mut resolver = FixedResolver::open(dest.path());
        resolver.authorized_members = vec![Member {
            principal: "alice".to_owned(),
            valid_after: None,
            valid_before: None,
            trust: Trust::Keys(Default::default()),
            provenance: Provenance::AdminRegistered,
            account: None,
            role: None,
        }];
        let backend = NativeBackend::new(resolver, signer);

        let push = PushRequest {
            repo: crate::RepoId::new("repo"),
            ref_edits: vec![RefEdit {
                name: RefName::new("refs/heads/main"),
                expected: Expected::MustNotExist,
                new: None,
            }],
            pack: PackStream::new(std::io::Cursor::new(empty_pack())),
            push_cert: None,
        };
        let outcome = backend.receive(push).unwrap();
        assert!(matches!(outcome, PushOutcome::Rejected { .. }));
    }

    #[test]
    fn rejects_a_push_with_a_missing_object() {
        let dest = bare_repo();
        let (_key_dir, signer) = test_signer();
        let backend = NativeBackend::new(FixedResolver::open(dest.path()), signer);

        let bogus =
            gix_hash::ObjectId::from_hex(b"1111111111111111111111111111111111111111").unwrap();
        let push = PushRequest {
            repo: crate::RepoId::new("repo"),
            ref_edits: vec![RefEdit {
                name: RefName::new("refs/heads/main"),
                expected: Expected::MustNotExist,
                new: Some(bogus),
            }],
            pack: PackStream::new(std::io::Cursor::new(empty_pack())),
            push_cert: None,
        };
        let outcome = backend.receive(push).unwrap();
        assert!(matches!(outcome, PushOutcome::Rejected { .. }));
    }

    #[test]
    fn accepts_a_signed_push_and_chains_the_op_record() {
        let source = bare_repo();
        let commit = commit_onto(source.path(), "file", "content");
        let pack = pack_for(source.path(), commit);

        let dest = bare_repo();
        let (_key_dir, signer) = test_signer();

        // Enroll a real member from a freshly generated key, and sign a
        // push certificate for it exactly as `git push --signed` would
        // produce (payload, then an `ssh-keygen -Y sign` signature block).
        let keys_dir = tempfile::tempdir().unwrap();
        let member_key = keys_dir.path().join("member_key");
        assert!(
            std::process::Command::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-f"])
                .arg(&member_key)
                .status()
                .unwrap()
                .success()
        );
        let public_key = std::fs::read_to_string(keys_dir.path().join("member_key.pub")).unwrap();

        let payload = "certificate version 0.1\npusher alice <alice@example.com>\npushee dest\nnonce \n\n0000000000000000000000000000000000000000 e69de29bb2d1d6434b8b29ae775ad8c2e48c5391 refs/heads/main\n";
        let payload_path = keys_dir.path().join("payload");
        std::fs::write(&payload_path, payload).unwrap();
        assert!(
            std::process::Command::new("ssh-keygen")
                .args(["-Y", "sign", "-n", "git", "-f"])
                .arg(&member_key)
                .arg(&payload_path)
                .status()
                .unwrap()
                .success()
        );
        let signature = std::fs::read_to_string(keys_dir.path().join("payload.sig")).unwrap();
        let certificate = format!("{payload}{signature}");

        let mut resolver = FixedResolver::open(dest.path());
        resolver.authorized_members = vec![Member {
            principal: "alice".to_owned(),
            valid_after: None,
            valid_before: None,
            trust: Trust::Keys(std::iter::once(("fp1".to_owned(), public_key)).collect()),
            provenance: Provenance::AdminRegistered,
            account: None,
            role: None,
        }];
        let backend = NativeBackend::new(resolver, signer);

        let push = PushRequest {
            repo: crate::RepoId::new("repo"),
            ref_edits: vec![RefEdit {
                name: RefName::new("refs/heads/main"),
                expected: Expected::MustNotExist,
                new: Some(commit),
            }],
            pack: PackStream::new(std::io::Cursor::new(pack)),
            push_cert: Some(PushCertificate::new(certificate)),
        };
        let outcome = backend.receive(push).unwrap();
        assert!(
            matches!(outcome, PushOutcome::Accepted { .. }),
            "{outcome:?}"
        );
    }
}
