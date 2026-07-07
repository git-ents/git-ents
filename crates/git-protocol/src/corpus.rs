//! The op-replay corpus (`docs/scale-out.adoc`, WS0's "op replay corpus",
//! feeding WS2's conformance suite): one [`CorpusEntry`] per accepted push,
//! carrying enough to replay it against any `RefStore`/`ObjectStore` pair and
//! assert an identical outcome — same final refs, same reachable object set.
//!
//! A corpus entry deliberately does *not* carry the op record itself (the
//! server-signed commit chained under [`crate::attestation::OP_LOG_REF`]):
//! that record embeds a wall-clock timestamp and a fresh signature, so
//! replaying it would never hash-match the original, and the audit trail it
//! represents is a server-internal concern orthogonal to "did this push
//! reproduce the same content." What a replay must reproduce is the
//! client-visible outcome: the ref edits the push asked for, applied in the
//! same order, over the same pack.

use gix_hash::ObjectId;

use crate::types::AppliedRefEdit;

/// One accepted push, durably logged so it can be replayed later against a
/// different `RefStore`/`ObjectStore` pair as a conformance fixture
/// (`docs/scale-out.adoc`, WS0: "every push logs (push-cert OID, ref edits
/// old/new, pack OIDs)").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorpusEntry {
    /// The client's push certificate, embedded by OID — `None` only during
    /// the bootstrap window (no members enrolled yet, so no certificate was
    /// required). Informational (the push's *intent*): not needed to
    /// replay the ref/object outcome, only to audit it.
    pub push_cert_oid: Option<ObjectId>,
    /// The ref edits this push applied, old and new — the push's *outcome*,
    /// and what a replay must reproduce exactly.
    pub ref_edits: Vec<AppliedRefEdit>,
    /// The pack introducing every object this push's ref edits need that
    /// the repository didn't already have. May be an empty pack (e.g. a
    /// pure ref deletion).
    pub pack: Vec<u8>,
}

impl CorpusEntry {
    /// Build an entry from a push's certificate bytes (already hashed the
    /// same way [`crate::native::ingest`] hashes an incoming certificate
    /// blob), its applied ref edits, and the pack that carried its new
    /// objects.
    #[must_use]
    pub fn new(
        push_cert_oid: Option<ObjectId>,
        ref_edits: Vec<AppliedRefEdit>,
        pack: Vec<u8>,
    ) -> Self {
        Self {
            push_cert_oid,
            ref_edits,
            pack,
        }
    }
}
