//! Refname namespaces under `refs/meta/*`.
//!
//! Every builder here composes a refname and validates it through gitoxide's
//! own [`gix::refs::FullName`] (`arch.no-object-store-trait`'s sibling rule:
//! never define a parallel refname type). [`classify`] is the inverse
//! direction — given a refname, which entity's namespace it falls in — for
//! callers (the gate, `receive`) that need to route on a pushed ref without
//! duplicating this module's namespace table.
//!
//! Spec coverage: `meta-ref.namespace`, `meta-ref.granularity`,
//! `meta-ref.inbox`, plus the `refs/meta/toolchains/*`
//! (`model.toolchain`) and `refs/meta/redactions/*` (`model.redaction`)
//! namespaces.

use gix::refs::{FullName, FullNameRef};

use crate::member::MemberId;
use crate::{Error, Result};

fn build(name: String) -> Result<FullName> {
    FullName::try_from(name.clone()).map_err(|source| Error::InvalidRefName { name, source })
}

/// The fixed ref for repository-global account state (`meta-ref.granularity`:
/// "Repository-global state with a single writer-of-record MUST instead live
/// on one fixed ref").
pub const ACCOUNT_REF: &str = "refs/meta/account";

/// The fixed ref for repository-global configuration
/// (`meta-ref.granularity`).
pub const CONFIG_REF: &str = "refs/meta/config";

/// The ref holding the member named `id` — `refs/meta/member/<id>`
/// (`meta-ref.granularity`).
///
/// # Examples
///
/// ```
/// use ents_model::{MemberId, namespace};
///
/// let name = namespace::member_ref(&MemberId::new("jdc")).expect("valid id");
/// assert_eq!(name.as_bstr(), "refs/meta/member/jdc");
/// ```
// @relation(meta-ref.granularity, scope=function)
pub fn member_ref(id: &MemberId) -> Result<FullName> {
    build(format!("refs/meta/member/{id}"))
}

/// The ref holding the issue named `id` — `refs/meta/issues/<id>`
/// (`meta-ref.granularity`).
// @relation(meta-ref.granularity, scope=function)
pub fn issue_ref(id: &str) -> Result<FullName> {
    build(format!("refs/meta/issues/{id}"))
}

/// The ref holding the comment named `id` — `refs/meta/comments/<id>`
/// (`meta-ref.granularity`).
// @relation(meta-ref.granularity, scope=function)
pub fn comment_ref(id: &str) -> Result<FullName> {
    build(format!("refs/meta/comments/{id}"))
}

/// The ref holding the effect named `name` — `refs/meta/effects/<name>`
/// (`meta-ref.granularity`).
// @relation(meta-ref.granularity, scope=function)
pub fn effect_ref(name: &str) -> Result<FullName> {
    build(format!("refs/meta/effects/{name}"))
}

/// The canonical ref for one effect's result on one tested commit —
/// `refs/meta/results/<effect>/<short_oid>` (`meta-ref.granularity`).
// @relation(meta-ref.granularity, scope=function)
pub fn result_ref(effect: &str, short_oid: &str) -> Result<FullName> {
    build(format!("refs/meta/results/{effect}/{short_oid}"))
}

/// The ref mirroring one effect's result that `member` produced on their own
/// executor — `refs/meta/self/<member>/<effect>/<short_oid>`
/// (`meta-ref.inbox`, `effect.self-run`).
///
/// `self` is its own top-level namespace, a fixed segment from the spec's
/// namespace table, so the canonical results glob
/// (`refs/meta/results/<effect>/*`) and the self-run glob
/// (`refs/meta/self/<member>/*`) are disjoint by construction.
///
/// # Examples
///
/// ```
/// use ents_model::{MemberId, namespace};
///
/// let name = namespace::self_result_ref(&MemberId::new("jdc"), "unit", "abc123")
///     .expect("valid segments");
/// assert_eq!(name.as_bstr(), "refs/meta/self/jdc/unit/abc123");
/// ```
// @relation(meta-ref.inbox, scope=function)
pub fn self_result_ref(member: &MemberId, effect: &str, short_oid: &str) -> Result<FullName> {
    build(format!("refs/meta/self/{member}/{effect}/{short_oid}"))
}

/// The member segment of a `refs/meta/self/<member>/...` refname, or `None`
/// when `name` is not under the self-run namespace (`meta-ref.inbox`).
///
/// The gate keys self-run authorization on this segment — a member may write
/// only their *own* self-run mirror — so it is extracted here, next to the
/// namespace table it belongs to, rather than re-parsed by every caller.
///
/// # Examples
///
/// ```
/// use ents_model::{MemberId, namespace};
///
/// let name: gix::refs::FullName = "refs/meta/self/jdc/unit/abc123".try_into().expect("valid");
/// assert_eq!(namespace::self_run_owner(name.as_ref()), Some(MemberId::new("jdc")));
///
/// let canonical: gix::refs::FullName = "refs/meta/results/unit/abc123".try_into().expect("valid");
/// assert_eq!(namespace::self_run_owner(canonical.as_ref()), None);
/// ```
// @relation(meta-ref.inbox, scope=function)
#[must_use]
pub fn self_run_owner(name: &FullNameRef) -> Option<MemberId> {
    let path = name.as_bstr().to_string();
    let rest = path.strip_prefix("refs/meta/self/")?;
    let (member, _) = rest.split_once('/')?;
    Some(MemberId::new(member))
}

/// The ref holding an inbox entity awaiting adoption —
/// `refs/meta/inbox/<id>` (`meta-ref.inbox`).
// @relation(meta-ref.inbox, scope=function)
pub fn inbox_ref(id: &str) -> Result<FullName> {
    build(format!("refs/meta/inbox/{id}"))
}

/// The ref holding the toolchain manifest named `name` —
/// `refs/meta/toolchains/<name>` (`model.toolchain`).
// @relation(model.toolchain, scope=function)
pub fn toolchain_ref(name: &str) -> Result<FullName> {
    build(format!("refs/meta/toolchains/{name}"))
}

/// The ref holding the redaction record named `id` —
/// `refs/meta/redactions/<id>` (`model.redaction`).
// @relation(model.redaction, scope=function)
pub fn redaction_ref(id: &str) -> Result<FullName> {
    build(format!("refs/meta/redactions/{id}"))
}

/// Which entity namespace a `refs/meta/*` refname falls in.
///
/// The inbox and self-run namespaces classify as their own variants even
/// though `meta-ref.inbox` requires them to "hold the same typed trees as
/// their canonical counterparts; only the refname rule differs" — that
/// refname rule is exactly what the gate routes on, so the distinction
/// belongs in this table rather than re-derived by every caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Namespace {
    /// `refs/meta/member/*`.
    Member,
    /// `refs/meta/issues/*`.
    Issue,
    /// `refs/meta/comments/*`.
    Comment,
    /// `refs/meta/effects/*`.
    Effect,
    /// `refs/meta/results/*` — canonical results only; a member's self-run
    /// mirror is [`Namespace::SelfRun`], disjoint by construction
    /// (`meta-ref.inbox`).
    Result,
    /// `refs/meta/self/<member>/*` — self-run result mirrors
    /// (`meta-ref.inbox`, [`self_result_ref`]).
    SelfRun,
    /// `refs/meta/toolchains/*`.
    Toolchain,
    /// `refs/meta/redactions/*`.
    Redaction,
    /// `refs/meta/inbox/*` — general inbox entities awaiting adoption.
    Inbox,
    /// The fixed `refs/meta/account` ref.
    Account,
    /// The fixed `refs/meta/config` ref.
    Config,
    /// Under `refs/meta/*`, but in no namespace this build of the vocabulary
    /// knows. `model.extensibility` requires a stock server to carry entity
    /// types it cannot parse, so the gate and `receive` must be able to
    /// route an unknown meta namespace generically rather than confuse it
    /// with a ref that is not forge state at all — which is why this is a
    /// variant and not a `None`.
    Unknown,
}

/// Classify a `refs/meta/*` refname by which entity's namespace it falls in.
///
/// Returns `None` only when `name` is not under `refs/meta/*` at all
/// (`meta-ref.namespace`: "All forge state MUST live under `refs/meta/*`").
/// A refname under `refs/meta/*` whose namespace this build does not know
/// classifies as [`Namespace::Unknown`] instead — it is still forge state
/// (`model.extensibility`), just state this vocabulary cannot interpret.
///
/// # Examples
///
/// ```
/// use ents_model::namespace::{self, Namespace};
///
/// let name: gix::refs::FullName = "refs/meta/issues/42".try_into().expect("valid");
/// assert_eq!(namespace::classify(name.as_ref()), Some(Namespace::Issue));
///
/// let outside: gix::refs::FullName = "refs/heads/main".try_into().expect("valid");
/// assert_eq!(namespace::classify(outside.as_ref()), None);
///
/// let novel: gix::refs::FullName = "refs/meta/reviews/7".try_into().expect("valid");
/// assert_eq!(namespace::classify(novel.as_ref()), Some(Namespace::Unknown));
/// ```
// @relation(meta-ref.namespace, meta-ref.granularity, model.extensibility, scope=function)
#[must_use]
pub fn classify(name: &FullNameRef) -> Option<Namespace> {
    let path = name.as_bstr().to_string();
    let rest = path.strip_prefix("refs/meta/")?;

    if rest == "account" {
        return Some(Namespace::Account);
    }
    if rest == "config" {
        return Some(Namespace::Config);
    }
    let (segment, _) = rest.split_once('/').unwrap_or((rest, ""));
    match segment {
        "member" => Some(Namespace::Member),
        "issues" => Some(Namespace::Issue),
        "comments" => Some(Namespace::Comment),
        "effects" => Some(Namespace::Effect),
        "results" => Some(Namespace::Result),
        "self" => Some(Namespace::SelfRun),
        "toolchains" => Some(Namespace::Toolchain),
        "redactions" => Some(Namespace::Redaction),
        "inbox" => Some(Namespace::Inbox),
        _ => Some(Namespace::Unknown),
    }
}

/// Whether a `refs/meta/*` refname names an inbox entity —
/// `refs/meta/inbox/*` — per `meta-ref.inbox`.
///
/// A member's self-run result mirror lives under its own top-level
/// `refs/meta/self/*` namespace ([`self_result_ref`]), not under the inbox,
/// so it is deliberately not matched here.
///
/// # Examples
///
/// ```
/// use ents_model::namespace;
///
/// let inbox: gix::refs::FullName = "refs/meta/inbox/abc".try_into().expect("valid");
/// assert!(namespace::is_inbox(inbox.as_ref()));
///
/// let canonical: gix::refs::FullName = "refs/meta/results/unit/abc123".try_into().expect("valid");
/// assert!(!namespace::is_inbox(canonical.as_ref()));
/// ```
// @relation(meta-ref.inbox, scope=function)
#[must_use]
pub fn is_inbox(name: &FullNameRef) -> bool {
    let path = name.as_bstr().to_string();
    let Some(rest) = path.strip_prefix("refs/meta/") else {
        return false;
    };
    rest.starts_with("inbox/")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "unit test")]

    use rstest::rstest;

    use super::*;

    fn name(s: &str) -> FullName {
        s.try_into().expect("valid refname in test table")
    }

    #[rstest]
    #[case::member("refs/meta/member/jdc", Some(Namespace::Member))]
    #[case::issue("refs/meta/issues/42", Some(Namespace::Issue))]
    #[case::comment("refs/meta/comments/abc", Some(Namespace::Comment))]
    #[case::effect("refs/meta/effects/unit", Some(Namespace::Effect))]
    #[case::result("refs/meta/results/unit/abc123", Some(Namespace::Result))]
    #[case::self_run("refs/meta/self/jdc/unit/abc123", Some(Namespace::SelfRun))]
    #[case::toolchain("refs/meta/toolchains/rust-stable", Some(Namespace::Toolchain))]
    #[case::redaction("refs/meta/redactions/abc", Some(Namespace::Redaction))]
    #[case::inbox("refs/meta/inbox/abc", Some(Namespace::Inbox))]
    #[case::account("refs/meta/account", Some(Namespace::Account))]
    #[case::config("refs/meta/config", Some(Namespace::Config))]
    #[case::outside_meta("refs/heads/main", None)]
    #[case::unrecognized("refs/meta/index/abc", Some(Namespace::Unknown))]
    #[case::novel_namespace("refs/meta/reviews/7", Some(Namespace::Unknown))]
    // @relation(meta-ref.namespace, meta-ref.granularity, scope=function, role=Verifies)
    fn classify_matches_the_namespace_table(
        #[case] refname: &str,
        #[case] expected: Option<Namespace>,
    ) {
        assert_eq!(classify(name(refname).as_ref()), expected);
    }

    #[rstest]
    #[case::inbox_entity("refs/meta/inbox/abc", true)]
    #[case::canonical_result("refs/meta/results/unit/abc123", false)]
    #[case::self_run_mirror("refs/meta/self/jdc/unit/abc123", false)]
    #[case::member("refs/meta/member/jdc", false)]
    // @relation(meta-ref.inbox, scope=function, role=Verifies)
    fn is_inbox_matches_only_inbox_namespaces(#[case] refname: &str, #[case] expected: bool) {
        assert_eq!(is_inbox(name(refname).as_ref()), expected);
    }

    #[rstest]
    #[case::self_run("refs/meta/self/jdc/unit/abc123", Some("jdc"))]
    #[case::self_run_deep_effect("refs/meta/self/worker-1/it/deadbeef", Some("worker-1"))]
    #[case::bare_self_segment("refs/meta/self/jdc", None)]
    #[case::canonical_result("refs/meta/results/unit/abc123", None)]
    #[case::outside_meta("refs/heads/main", None)]
    // @relation(meta-ref.inbox, scope=function, role=Verifies)
    fn self_run_owner_extracts_only_the_self_namespace_member(
        #[case] refname: &str,
        #[case] expected: Option<&str>,
    ) {
        assert_eq!(
            self_run_owner(name(refname).as_ref()),
            expected.map(MemberId::new)
        );
    }

    #[rstest]
    // @relation(meta-ref.namespace, scope=function, role=Verifies)
    fn every_builder_stays_under_refs_meta() {
        let id = MemberId::new("jdc");
        let built = [
            member_ref(&id).expect("valid"),
            issue_ref("42").expect("valid"),
            comment_ref("abc").expect("valid"),
            effect_ref("unit").expect("valid"),
            result_ref("unit", "abc123").expect("valid"),
            self_result_ref(&id, "unit", "abc123").expect("valid"),
            inbox_ref("abc").expect("valid"),
            toolchain_ref("rust-stable").expect("valid"),
            redaction_ref("abc").expect("valid"),
        ];
        for name in built {
            assert!(
                name.as_bstr().starts_with(b"refs/meta/"),
                "{name} must live under refs/meta/*"
            );
        }
    }

    #[rstest]
    // @relation(meta-ref.namespace, scope=function, role=Verifies)
    fn invalid_component_is_rejected_not_silently_accepted() {
        let err = issue_ref("../escape").expect_err("must reject a refname with a `..` component");
        assert!(matches!(err, Error::InvalidRefName { .. }));
    }
}
