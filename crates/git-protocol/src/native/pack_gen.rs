//! [`GeneratePack`] on the storage traits: read [`PackPlan`]'s objects back
//! one at a time and encode them as full base objects (`crate::pack`).
//! Correctness-first, not space-efficient — see the trait's own doc comment
//! and `docs/scale-out.adoc`'s Q6.

use git_backend::PackStream;

use super::{BackendResolver, NativeBackend};
use crate::pack::{PackObject, build_pack};
use crate::types::PackPlan;
use crate::{GeneratePack, Result};

impl<R: BackendResolver> GeneratePack for NativeBackend<R> {
    fn stream(&self, plan: &PackPlan) -> Result<PackStream> {
        let backends = self.backends(&plan.repo)?;
        let mut objects = Vec::with_capacity(plan.objects.len());
        for id in &plan.objects {
            let object = backends.objects.read(*id)?;
            objects.push(PackObject {
                id: *id,
                kind: object.kind,
                data: object.data,
            });
        }
        let bytes = build_pack(&objects)?;
        Ok(PackStream::new(std::io::Cursor::new(bytes)))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "test fixture")]

    use std::io::Read as _;

    use super::*;
    use crate::Negotiate as _;
    use crate::native::NativeBackend;
    use crate::native::test_support::{FixedResolver, bare_repo, commit_onto, test_signer};
    use crate::types::NegotiationState;

    #[test]
    fn generates_a_pack_git_index_pack_accepts() {
        let bare = bare_repo();
        let commit = commit_onto(bare.path(), "file", "content");
        let (_key_dir, signer) = test_signer();
        let backend = NativeBackend::new(FixedResolver::open(bare.path()), signer);

        let mut session = NegotiationState {
            repo: crate::RepoId::new("repo"),
            wants: vec![commit],
            haves: Vec::new(),
        };
        let plan = backend.wants_haves(&mut session).unwrap();
        let mut stream = backend.stream(&plan).unwrap();
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).unwrap();

        let dest = tempfile::tempdir().unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "-q", "--bare"])
            .arg(dest.path())
            .status()
            .unwrap();
        assert!(status.success());
        let mut child = std::process::Command::new("git")
            .arg("-C")
            .arg(dest.path())
            .args(["index-pack", "--stdin"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        {
            use std::io::Write as _;
            child.stdin.take().unwrap().write_all(&bytes).unwrap();
        }
        assert!(child.wait().unwrap().success());
    }
}
