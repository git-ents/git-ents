//! Translating a reached [`ents_receive::Outcome`] into a CLI-facing
//! result: the commit-building mechanism itself
//! ([`ents_receive::propose_entity`], [`ents_receive::propose_delete`],
//! [`ents_receive::Identity`]) moved to `ents-receive` (kernel material,
//! shared by every mutation frontend — including `ents-forge`'s comment
//! command — not CLI-specific); this module keeps only the one thing that
//! is genuinely CLI-only: rendering a failure for a human.

use ents_receive::{Outcome, TxResult};
use gix_hash::ObjectId;

use crate::error::{Error, Result};

/// Translate a reached [`Outcome`] into `Ok(tip)` on success or a
/// user-facing [`Error`] otherwise — the one place every command renders
/// `receive`'s result the same way.
///
/// # Errors
///
/// [`Error::Refused`] for a gate refusal (`gate.mandatory-hosted`
/// aborting on any failed verdict, or an advisory root's failed verdict a
/// caller chose to treat as fatal); [`Error::Stale`] for a compare-and-swap
/// rejection; [`Error::Redacted`] if a redacted object was refused.
pub fn outcome_to_result(outcome: Outcome, tip: Option<ObjectId>) -> Result<Option<ObjectId>> {
    match outcome.result {
        TxResult::Applied => Ok(tip),
        TxResult::Refused => {
            let reasons = outcome
                .verdicts
                .iter()
                .filter_map(|(_, verdict)| match verdict {
                    ents_gate::Verdict::Fail(refusal) => Some(refusal.to_string()),
                    ents_gate::Verdict::Pass(_) => None,
                })
                .collect::<Vec<_>>()
                .join("; ");
            Err(Error::Refused(reasons))
        }
        TxResult::Rejected { name } => Err(Error::Stale {
            name: name.as_bstr().to_string(),
        }),
        TxResult::Redacted { oid } => Err(Error::Redacted { oid }),
    }
}
