//! The gate's verdict vocabulary: admission, refusal, and the
//! machine-readable reason a refusal carries (`gate.verdict-reason`).

use gix::refs::FullName;
use gix_ref_store::Expected;

/// The requirement a refusal names — one of the tip-invariant rules
/// `gate.tip-signed` through `gate.atomic-cas`, exactly the range
/// `gate.verdict-reason` requires a failure to identify.
///
/// [`Requirement::AtomicCas`] is never produced by [`crate::verify`]
/// itself (the gate reads, it does not write); it exists so the caller
/// that *does* run the compare-and-swap can report a stale-precondition
/// rejection in the same vocabulary.
///
/// # Examples
///
/// ```
/// use ents_gate::Requirement;
///
/// assert_eq!(Requirement::TipSigned.uid(), "gate.tip-signed");
/// assert_eq!(Requirement::FastForward.uid(), "gate.fast-forward");
/// ```
// @relation(gate.verdict-reason, scope=file)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Requirement {
    /// `gate.tip-signed`: the new tip must be signed by a member
    /// authorized for the refname.
    TipSigned,
    /// `gate.refname-binding`: the commit's `Ents-Ref:` trailer must
    /// match the refname being updated.
    RefnameBinding,
    /// `gate.fast-forward`: the new tip must descend from the old tip.
    FastForward,
    /// `gate.atomic-cas`: the update must commit via compare-and-swap
    /// against the old tip the gate read.
    AtomicCas,
}

impl Requirement {
    /// The spec requirement id this variant names.
    #[must_use]
    pub fn uid(&self) -> &'static str {
        match self {
            Self::TipSigned => "gate.tip-signed",
            Self::RefnameBinding => "gate.refname-binding",
            Self::FastForward => "gate.fast-forward",
            Self::AtomicCas => "gate.atomic-cas",
        }
    }
}

/// Why a passing update passed — advisory call sites render this, so a
/// local UI can say "admitted under the bootstrap window" rather than a
/// bare yes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionKind {
    /// The full tip invariant held (`gate.tip-signed` through
    /// `gate.fast-forward`, with the CAS precondition attached).
    TipInvariant,
    /// No verification epoch is recorded in `refs/meta/config`, and this
    /// update does not set one: the tip invariant is not yet in force
    /// (`gate.epoch` — history before the epoch is archival).
    PreEpoch,
    /// Admitted by the empty-member-list bootstrap window: a first
    /// enrollment, self-admitting (`gate.bootstrap`).
    Bootstrap,
    /// The refname is outside `refs/meta/*`: branch and tag refs keep
    /// transport-level authorization instead of the tip invariant
    /// (`gate.principled-split`).
    CodeRef,
}

/// A passing verdict: the update may proceed, and `cas` is the
/// compare-and-swap precondition the write MUST use — bound to the same
/// old-tip read the fast-forward check used, which is what makes the
/// eventual ref update atomic against races (`gate.atomic-cas`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Admission {
    /// Why the update passed.
    pub kind: AdmissionKind,
    /// The refname judged.
    pub refname: FullName,
    /// The CAS precondition for the write: `MustExistAndMatch(old tip)`
    /// when the ref existed at verification time, `MustNotExist` when it
    /// did not.
    pub cas: Expected,
}

/// A failing verdict: which requirement failed, for which refname, and a
/// rendered reason (`gate.verdict-reason` — never a bare pass/fail).
///
/// # Examples
///
/// ```
/// use ents_gate::{Refusal, Requirement};
///
/// let refusal = Refusal {
///     requirement: Requirement::TipSigned,
///     refname: "refs/meta/issues/42".try_into().expect("valid"),
///     detail: "your signing key is not authorized for this ref".into(),
///     inbox_alternative: true,
/// };
/// let rendered = refusal.to_string();
/// assert!(rendered.contains("gate.tip-signed"));
/// assert!(rendered.contains("refs/meta/inbox"));
/// ```
// @relation(gate.verdict-reason, scope=file)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// The tip-invariant rule that failed.
    pub requirement: Requirement,
    /// The refname the update targeted.
    pub refname: FullName,
    /// A human-readable, actionable reason.
    pub detail: String,
    /// Whether submitting through the inbox namespace would be accepted
    /// instead — set on authorization refusals so advisory call sites can
    /// surface the inbox alternative at verdict time, not only once a
    /// push is rejected (`gate.advisory-local`, `sync.inbox-routing`).
    pub inbox_alternative: bool,
}

impl std::fmt::Display for Refusal {
    // @relation(gate.verdict-reason, gate.advisory-local, scope=function)
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} (rule {}, ref {})",
            self.detail,
            self.requirement.uid(),
            self.refname.as_bstr()
        )?;
        if self.inbox_alternative {
            write!(
                f,
                "; you can still submit this change under your own refs/meta/inbox/<member>/* segment for adoption by an authorized member"
            )?;
        }
        Ok(())
    }
}

/// The gate's verdict on one proposed ref update.
///
/// The same value is computed at all three call sites
/// (`gate.call-sites`); what differs is only what the caller does with a
/// [`Verdict::Fail`] — abort the transaction (hosted CAS,
/// `gate.mandatory-hosted`) or annotate and proceed (local UI and push
/// pre-flight, `gate.advisory-local`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The update satisfies the gate; write it with
    /// [`Admission::cas`] as the precondition.
    Pass(Admission),
    /// The update violates the tip invariant; the refusal says which
    /// rule, for which ref, and why.
    Fail(Refusal),
}

impl Verdict {
    /// Whether this verdict admits the update.
    ///
    /// # Examples
    ///
    /// ```
    /// use ents_gate::{Admission, AdmissionKind, Verdict};
    /// use gix_ref_store::Expected;
    ///
    /// let verdict = Verdict::Pass(Admission {
    ///     kind: AdmissionKind::CodeRef,
    ///     refname: "refs/heads/main".try_into().expect("valid"),
    ///     cas: Expected::MustNotExist,
    /// });
    /// assert!(verdict.is_pass());
    /// ```
    #[must_use]
    pub fn is_pass(&self) -> bool {
        matches!(self, Self::Pass(_))
    }
}
