//! WS3: the protocol traits and their native implementation
//! (`docs/scale-out.adoc`, "Protocol traits", "Attested push", "Correctness
//! rules", "WS3").
//!
//! The server *is* four traits — [`Advertise`], [`Negotiate`],
//! [`GeneratePack`], [`IngestPack`] — defined in [`traits`] against the
//! minimal types in [`types`]. [`native`] implements all four on the
//! storage traits from `git-backend`: advertisement from
//! `RefStore::iter_prefix`, negotiation and connectivity checking as a
//! shared reachability walk ([`walk`]) over `ObjectStore::read`, pack
//! generation via [`pack`]'s whole-object encoder, and ingest with the
//! staged-then-atomically-committed-then-promoted ordering the correctness
//! rules require.
//!
//! [`attestation`] is the other half of "Attested push": uniform-strong
//! attestation (every push needs a client-signed push certificate, checked
//! by extending `git-signed-push`'s verifier), the namespace attestation
//! policy (currently pinned everywhere to `client-cert-required`), and the
//! server-signed op record every accepted push emits.

pub mod attestation;
pub mod native;
pub mod pack;
mod traits;
pub mod types;
pub mod walk;

pub use traits::{Advertise, GeneratePack, IngestPack, Negotiate};
pub use types::{
    AdSpec, AppliedRefEdit, NegotiationState, PackPlan, PushCertificate, PushOutcome, PushRequest,
    RefAdvertisement, RepoId,
};

/// A failure in a protocol trait implementation.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The underlying storage traits reported a failure.
    #[error(transparent)]
    Backend(#[from] git_backend::Error),
    /// A connectivity check found an object neither the incoming pack nor
    /// the promoted object store could resolve.
    #[error("missing object {0}: push rejected, connectivity check failed")]
    MissingObject(gix_hash::ObjectId),
    /// The reachability walk (`git-reachability`, WS6) itself failed —
    /// distinct from [`Self::MissingObject`], which is this crate's own
    /// mapping of that same failure at the negotiation/ingest call sites
    /// that need a typed reason to report back to a client.
    #[error(transparent)]
    Reachability(#[from] git_reachability::Error),
    /// Decoding a commit, tree, or tag object failed.
    #[error("could not decode object: {0}")]
    Decode(String),
    /// Encoding or writing a pack failed.
    #[error("pack error: {0}")]
    Pack(String),
    /// Attestation (push certificate verification, or namespace policy)
    /// failed.
    #[error("push rejected: {0}")]
    Attestation(String),
    /// The requested repository is not known to the resolver in use.
    #[error("unknown repository {0:?}")]
    UnknownRepo(String),
    /// An underlying I/O error.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// This crate's `Result` alias.
pub type Result<T> = std::result::Result<T, Error>;
