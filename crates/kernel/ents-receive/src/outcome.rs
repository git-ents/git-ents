//! `receive`'s gate policy (mandatory or advisory) and the outcome it
//! reports: which verdict each proposed transition got, and whether the
//! batch actually landed.

use gix::refs::FullName;
use gix_hash::ObjectId;

use ents_gate::Verdict;

/// Which of the two gate policies `receive.adoc` names governs one call:
/// abort the whole batch on a failing verdict (`gate.mandatory-hosted`), or
/// accept the write regardless and only annotate (`gate.advisory-local`).
///
/// The gate itself ([`ents_gate::verify`]) is one pure function evaluated
/// identically either way (`gate.call-sites`); `Mode` is the policy
/// [`crate::receive`] applies to a *failing* verdict, which is exactly the
/// orchestration the development plan assigns to this crate — the gate
/// crate never sees a `Mode`, and could not: it has no write path to gate.
///
/// # Examples
///
/// ```
/// use ents_receive::Mode;
///
/// let mode = Mode::Advisory;
/// assert_eq!(mode, Mode::Advisory);
/// ```
// @relation(gate.mandatory-hosted, gate.advisory-local, scope=file)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The hosted policy: a failing verdict against any transition in the
    /// batch aborts the whole batch before any ref is updated
    /// (`gate.mandatory-hosted`).
    Mandatory,
    /// The local policy: every transition is written regardless of its
    /// verdict; a failing verdict only annotates the result, never blocks
    /// it (`gate.advisory-local`).
    Advisory,
}

/// What happened to one [`crate::Proposal`]'s ref-transaction batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxResult {
    /// Every transition in the batch was written atomically.
    Applied,
    /// [`Mode::Mandatory`] aborted the whole batch before attempting a
    /// write, because at least one transition's verdict failed
    /// (`gate.mandatory-hosted`). See [`crate::Outcome::verdicts`] for
    /// which one and why.
    Refused,
    /// The underlying store rejected the compare-and-swap: `name`'s
    /// current value no longer matched the precondition read at
    /// evaluation time — a genuine race, reported in the gate's own
    /// vocabulary (`Requirement::AtomicCas`) as the gate crate's docs
    /// anticipate for exactly this caller.
    Rejected {
        /// The ref whose precondition was stale.
        name: FullName,
    },
    /// The batch introduced an object matching a previously recorded
    /// redaction target; the whole batch was refused before any verdict
    /// was even evaluated, so a redacted hole cannot be silently refilled
    /// by re-pushing the same bytes (`receive.redaction-ingest`).
    Redacted {
        /// The offending object id.
        oid: ObjectId,
    },
}

/// The result of one [`crate::receive`] call: every transition's verdict,
/// and what happened to the batch as a whole.
///
/// # Examples
///
/// ```
/// use ents_receive::{Outcome, TxResult};
///
/// let outcome = Outcome {
///     verdicts: vec![],
///     result: TxResult::Applied,
/// };
/// assert_eq!(outcome.result, TxResult::Applied);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// Each proposed transition's refname and the gate's verdict on it,
    /// in proposal order. Present under both [`Mode`]s: mandatory callers
    /// use it to see which refusal aborted the batch
    /// (`gate.verdict-reason`); advisory callers render it to the user
    /// regardless of [`Outcome::result`] (`gate.advisory-local`).
    pub verdicts: Vec<(FullName, Verdict)>,
    /// What happened to the batch.
    pub result: TxResult,
}
