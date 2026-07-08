//! The native backend: [`Advertise`](crate::Advertise),
//! [`Negotiate`](crate::Negotiate), [`GeneratePack`](crate::GeneratePack),
//! and [`IngestPack`](crate::IngestPack) implemented on the `git-backend`
//! storage traits, per `docs/scale-out.adoc`'s WS3.
//!
//! [`NativeBackend`] is generic over a [`BackendResolver`], which maps a
//! [`RepoId`] the wire layer names to the concrete `RefStore`/`ObjectStore`
//! pair (plus the trust set attested pushes check against) for that
//! repository — the server can host many repositories behind one
//! `NativeBackend`.

mod advertise;
mod ingest;
mod negotiate;
mod pack_gen;
#[cfg(test)]
mod test_support;

use std::sync::Arc;

use git_member::members::Member;

use crate::Result;
use crate::attestation::OpSigner;
use crate::types::RepoId;

/// The backends one repository resolves to: its ref and object stores, the
/// members currently trusted to sign a push (already revocation-filtered;
/// empty means the repository is still in its bootstrap window), and the
/// config their roles are checked against.
///
/// Loading `authorized_members`/`config` is deliberately the resolver's job,
/// not this crate's: it keeps `git-protocol` off `git-store`'s concrete
/// on-disk representation, which the storage-trait refactor (WS1) already
/// abstracted away for refs and objects but not yet for `git-member`'s data
/// (`docs/scale-out.adoc` doesn't ask WS3 to finish that; see the
/// simplification plan for the remaining P5/P6 work).
pub struct RepoBackends {
    /// The repository's ref store.
    pub refs: Arc<dyn git_backend::RefStore>,
    /// The repository's object store.
    pub objects: Arc<dyn git_backend::ObjectStore>,
    /// Members currently trusted to sign a push, revocations already
    /// subtracted. Empty means the bootstrap window is still open.
    pub authorized_members: Vec<Member>,
    /// The config `authorized_members`' roles are checked against.
    pub config: git_ents_core::config::Config,
    /// This repository's reachability artifacts (WS6), if any have been
    /// generated — [`gix_reachability::ArtifactBundle::empty`] for a
    /// resolver that hasn't wired artifact loading yet, which is exactly
    /// the "absent artifact" case negotiation/ingest must (and do) degrade
    /// gracefully from.
    pub reachability: gix_reachability::ArtifactBundle,
}

/// Resolves a [`RepoId`] to the backends that serve it.
pub trait BackendResolver: Send + Sync {
    /// The backends for `repo`, or `Err` if it names no known repository.
    fn resolve(&self, repo: &RepoId) -> Result<RepoBackends>;
}

/// The native implementation of all four protocol traits, parameterized
/// over how repositories resolve to backends ([`R`]) and how op records get
/// their server signature ([`OpSigner`]).
pub struct NativeBackend<R> {
    resolver: R,
    signer: Arc<dyn OpSigner>,
}

impl<R: BackendResolver> NativeBackend<R> {
    /// Build a native backend resolving repositories through `resolver` and
    /// signing op records with `signer`.
    pub fn new(resolver: R, signer: Arc<dyn OpSigner>) -> Self {
        Self { resolver, signer }
    }

    fn backends(&self, repo: &RepoId) -> Result<RepoBackends> {
        self.resolver.resolve(repo)
    }
}
