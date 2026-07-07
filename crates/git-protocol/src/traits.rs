//! The four protocol traits `docs/scale-out.adoc` ("Protocol traits") draws
//! between a git client and repository state: **the server *is* these four**.
//! Application/wire code (smart-HTTP handlers, a future SSH transport) is
//! written once against them; [`crate::native`] is one conforming
//! implementation — a stock-git-wrapped backend (`receive-pack` against a
//! scratch repo, say) is permitted to be another, per the doc's decision
//! record on protocol traits.

use git_backend::PackStream;

use crate::Result;
use crate::types::{
    AdSpec, NegotiationState, PackPlan, PushOutcome, PushRequest, RefAdvertisement, RepoId,
};

/// Advertise a repository's refs to a client, filtered by [`AdSpec`]. Backs
/// `GET .../info/refs` in smart-HTTP.
pub trait Advertise {
    /// The refs `filter` selects in `repo`, plus `HEAD`'s resolved tip.
    fn refs(&self, repo: &RepoId, filter: &AdSpec) -> Result<RefAdvertisement>;
}

/// Reduce a client's wants/haves to the exact object set a pack must carry.
/// Backs the negotiation phase of `git-upload-pack`.
///
/// # Contract
///
/// The returned [`PackPlan`] must contain every object reachable from
/// `session.wants` that is not reachable from `session.haves` — no more (a
/// client already holding an object should not receive it again) and no
/// less (a client missing an object must receive it, transitively).
pub trait Negotiate {
    /// Compute the [`PackPlan`] for `session`'s current wants/haves.
    fn wants_haves(&self, session: &mut NegotiationState) -> Result<PackPlan>;
}

/// Generate the pack a [`PackPlan`] describes. Backs the pack-generation
/// phase of `git-upload-pack`.
///
/// Pack generation over ranged reads (rather than a full local object store)
/// is the largest and riskiest custom component the development plan
/// identifies (`docs/scale-out.adoc`, Q6, WS3) — [`crate::native`]'s
/// implementation is correctness-first (whole objects read one at a time
/// through [`git_backend::ObjectStore::read`], no delta reuse); the
/// ranged-read optimization is WS5/WS6's job.
pub trait GeneratePack {
    /// Stream the pack `plan` describes.
    fn stream(&self, plan: &PackPlan) -> Result<PackStream>;
}

/// Ingest a push. Backs `git-receive-pack`, and is where every correctness
/// rule in `docs/scale-out.adoc` that governs a write converges: causal
/// collection safety, ref transactions as the only commit point, and
/// attested push as the only write path.
///
/// # Contract (ordering)
///
/// An implementation must, in order:
///
/// 1. Verify attestation (the client push certificate) before staging
///    anything, per the uniform-strong policy (`docs/scale-out.adoc`,
///    "Attested push").
/// 2. Stage the incoming pack's objects — invisible to reachability and GC
///    until promoted.
/// 3. Check connectivity: every object the ref edits' new tips need must
///    resolve, in the staged pack or the existing store.
/// 4. Commit every ref edit as **one** atomic transaction — the only commit
///    point (`docs/scale-out.adoc`, correctness rule 2) — including the op
///    record ref alongside the caller's edits.
/// 5. Promote the staged objects only after that transaction applies.
///
/// A push that fails any earlier step must not reach the ones after it, and
/// nothing it staged may become visible.
pub trait IngestPack {
    /// Ingest `push`, per the ordering contract above.
    fn receive(&self, push: PushRequest) -> Result<PushOutcome>;
}
