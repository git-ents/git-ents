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
//! (`model.toolchain`), `refs/meta/redactions/*` (`model.redaction`),
//! `refs/meta/reviews/*` (`model.review`), and `refs/meta/pins/*`
//! (`model.review-pin`) namespaces.

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

/// The ref holding one reviewer's review of one commit —
/// `refs/meta/reviews/<target>/<member>` (`meta-ref.granularity`,
/// `model.review`), where `<target>` is the oid of the first commit the
/// review judged and `<member>` is the reviewer's member id: a composite
/// natural key (`meta-ref.identity-binding`) with no minted id anywhere,
/// so one review thread lives per (target, reviewer) and all reviews of a
/// commit enumerate by ref prefix.
///
/// # Examples
///
/// ```
/// use ents_model::{MemberId, namespace};
///
/// let name = namespace::review_ref("deadbeef", &MemberId::new("jdc")).expect("valid");
/// assert_eq!(name.as_bstr(), "refs/meta/reviews/deadbeef/jdc");
/// ```
// @relation(meta-ref.granularity, model.review, meta-ref.identity-binding, scope=function)
pub fn review_ref(target: &str, member: &MemberId) -> Result<FullName> {
    build(format!("refs/meta/reviews/{target}/{member}"))
}

/// The retention pin for one reviewer's review of one commit —
/// `refs/meta/pins/reviews/<target>/<member>` (`model.review-pin`): the
/// entity's own canonical suffix (`reviews/<target>/<member>`) prefixed
/// with `pins/`, the same way `meta-ref.inbox` prefixes one, so two entity
/// kinds can never collide under the same pin id.
///
/// A pin ref's commits carry the empty tree, never an entity — the sole
/// exception to `meta-ref.namespace`'s tree-is-the-entity shape; the
/// commits exist purely to keep the reviewed commit and its ancestry
/// reachable. Because a pin's ancestry deliberately reaches into code
/// history, the gate's parentless-roots walk is never applied to a pin
/// (`meta-ref.identity-binding`).
///
/// # Examples
///
/// ```
/// use ents_model::{MemberId, namespace};
///
/// let name = namespace::review_pin_ref("deadbeef", &MemberId::new("jdc")).expect("valid");
/// assert_eq!(name.as_bstr(), "refs/meta/pins/reviews/deadbeef/jdc");
/// ```
// @relation(model.review-pin, meta-ref.namespace, meta-ref.identity-binding, scope=function)
pub fn review_pin_ref(target: &str, member: &MemberId) -> Result<FullName> {
    build(format!("refs/meta/pins/reviews/{target}/{member}"))
}

/// The `(target, member)` a review or review-pin refname names, or `None`
/// when `name` is not a well-formed `refs/meta/reviews/<target>/<member>`
/// or `refs/meta/pins/reviews/<target>/<member>` ref (`model.review`).
///
/// The gate recomputes a review's composite key from its signed content
/// and compares it to this parse (`meta-ref.identity-binding`,
/// `gate.identity-binding`), so the parser lives here next to the builder
/// rather than re-derived at the call site.
///
/// # Examples
///
/// ```
/// use ents_model::{MemberId, namespace};
///
/// let name: gix::refs::FullName = "refs/meta/reviews/deadbeef/jdc".try_into().expect("valid");
/// assert_eq!(
///     namespace::parse_review_ref(name.as_ref()),
///     Some(("deadbeef".to_owned(), MemberId::new("jdc"))),
/// );
///
/// let pin: gix::refs::FullName = "refs/meta/pins/reviews/deadbeef/jdc".try_into().expect("valid");
/// assert_eq!(
///     namespace::parse_review_ref(pin.as_ref()),
///     Some(("deadbeef".to_owned(), MemberId::new("jdc"))),
/// );
/// ```
// @relation(model.review, meta-ref.identity-binding, scope=function)
#[must_use]
pub fn parse_review_ref(name: &FullNameRef) -> Option<(String, MemberId)> {
    let path = name.as_bstr().to_string();
    let rest = path
        .strip_prefix("refs/meta/reviews/")
        .or_else(|| path.strip_prefix("refs/meta/pins/reviews/"))?;
    let (target, member) = rest.split_once('/')?;
    if target.is_empty() || member.is_empty() || member.contains('/') {
        return None;
    }
    Some((target.to_owned(), MemberId::new(member)))
}

/// The `(effect, short_oid)` a result refname names, or `None` when `name`
/// is not a well-formed `refs/meta/results/<effect>/<short-oid>` or
/// `refs/meta/self/<member>/<effect>/<short-oid>` ref
/// (`effect.results-writeback`, `meta-ref.inbox`).
///
/// The gate recomputes a result's composite key from its signed tree's
/// effect and target fields and compares it to this parse
/// (`model.result-identity`, `gate.identity-binding`).
///
/// # Examples
///
/// ```
/// use ents_model::namespace;
///
/// let name: gix::refs::FullName = "refs/meta/results/unit/abc123".try_into().expect("valid");
/// assert_eq!(
///     namespace::parse_result_ref(name.as_ref()),
///     Some(("unit".to_owned(), "abc123".to_owned())),
/// );
///
/// let self_run: gix::refs::FullName = "refs/meta/self/jdc/unit/abc123".try_into().expect("valid");
/// assert_eq!(
///     namespace::parse_result_ref(self_run.as_ref()),
///     Some(("unit".to_owned(), "abc123".to_owned())),
/// );
/// ```
// @relation(model.result-identity, meta-ref.identity-binding, scope=function)
#[must_use]
pub fn parse_result_ref(name: &FullNameRef) -> Option<(String, String)> {
    let path = name.as_bstr().to_string();
    let rest = path.strip_prefix("refs/meta/")?;
    let tail = if let Some(canonical) = rest.strip_prefix("results/") {
        canonical.to_owned()
    } else {
        let self_run = rest.strip_prefix("self/")?;
        // refs/meta/self/<member>/<effect>/<short-oid>: drop the member.
        let (_, effect_and_oid) = self_run.split_once('/')?;
        effect_and_oid.to_owned()
    };
    let (effect, short_oid) = tail.split_once('/')?;
    if effect.is_empty() || short_oid.is_empty() || short_oid.contains('/') {
        return None;
    }
    Some((effect.to_owned(), short_oid.to_owned()))
}

/// The ref holding the effect named `name` — `refs/meta/effects/<name>`
/// (`meta-ref.granularity`, `effect.definition`).
// @relation(meta-ref.granularity, effect.definition, scope=function)
pub fn effect_ref(name: &str) -> Result<FullName> {
    build(format!("refs/meta/effects/{name}"))
}

/// The canonical ref for one effect's result on one tested commit —
/// `refs/meta/results/<effect>/<short_oid>` (`meta-ref.granularity`,
/// `effect.definition`: derived from the effect's own name, never a
/// stored pattern).
// @relation(meta-ref.granularity, effect.definition, scope=function)
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

/// The ref holding one inbox entity authored by `member`, awaiting
/// adoption — `refs/meta/inbox/<member>/<id>` (`meta-ref.inbox`).
///
/// The member segment leads, symmetric with [`self_result_ref`], so the
/// gate's authorization keys off the refname alone: a member — either
/// provenance — may write only under its own segment, and nobody,
/// admins included, writes into another member's inbox.
///
/// # Examples
///
/// ```
/// use ents_model::{MemberId, namespace};
///
/// let name = namespace::inbox_ref(&MemberId::new("jdc"), "issue-42").expect("valid");
/// assert_eq!(name.as_bstr(), "refs/meta/inbox/jdc/issue-42");
/// ```
// @relation(meta-ref.inbox, scope=function)
pub fn inbox_ref(member: &MemberId, id: &str) -> Result<FullName> {
    build(format!("refs/meta/inbox/{member}/{id}"))
}

/// The member segment of a `refs/meta/inbox/<member>/...` refname, or
/// `None` when `name` is not under the inbox namespace or carries no
/// member segment (`meta-ref.inbox`).
///
/// Mirrors [`self_run_owner`]: the gate keys inbox authorization on this
/// segment, so it is extracted here, next to the namespace table.
///
/// # Examples
///
/// ```
/// use ents_model::{MemberId, namespace};
///
/// let name: gix::refs::FullName = "refs/meta/inbox/jdc/issue-42".try_into().expect("valid");
/// assert_eq!(namespace::inbox_owner(name.as_ref()), Some(MemberId::new("jdc")));
///
/// // The legacy unscoped shape has no owner to authorize.
/// let unscoped: gix::refs::FullName = "refs/meta/inbox/issue-42".try_into().expect("valid");
/// assert_eq!(namespace::inbox_owner(unscoped.as_ref()), None);
/// ```
// @relation(meta-ref.inbox, scope=function)
#[must_use]
pub fn inbox_owner(name: &FullNameRef) -> Option<MemberId> {
    let path = name.as_bstr().to_string();
    let rest = path.strip_prefix("refs/meta/inbox/")?;
    let (member, _) = rest.split_once('/')?;
    Some(MemberId::new(member))
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
    /// `refs/meta/reviews/*`.
    Review,
    /// `refs/meta/pins/*` — retention pins (`model.review-pin`,
    /// [`review_pin_ref`]): empty-tree commits anchoring other content's
    /// reachability, the sole exception to `meta-ref.namespace`'s
    /// tree-is-the-entity shape.
    Pin,
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
    /// `refs/meta/inbox/<member>/*` — entities awaiting adoption,
    /// each under its author's own segment (`meta-ref.inbox`,
    /// [`inbox_ref`], [`inbox_owner`]).
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
/// let novel: gix::refs::FullName = "refs/meta/widgets/7".try_into().expect("valid");
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
        "reviews" => Some(Namespace::Review),
        "pins" => Some(Namespace::Pin),
        "effects" => Some(Namespace::Effect),
        "results" => Some(Namespace::Result),
        "self" => Some(Namespace::SelfRun),
        "toolchains" => Some(Namespace::Toolchain),
        "redactions" => Some(Namespace::Redaction),
        "inbox" => Some(Namespace::Inbox),
        _ => Some(Namespace::Unknown),
    }
}

/// Whether a `refs/meta/*` refname is under the inbox namespace —
/// `refs/meta/inbox/<member>/*` — per `meta-ref.inbox`.
///
/// This is namespace membership only; which member owns the segment is
/// [`inbox_owner`]'s answer. A member's self-run result mirror lives
/// under its own top-level `refs/meta/self/*` namespace
/// ([`self_result_ref`]), not under the inbox, so it is deliberately not
/// matched here.
///
/// # Examples
///
/// ```
/// use ents_model::namespace;
///
/// let inbox: gix::refs::FullName = "refs/meta/inbox/jdc/issue-42".try_into().expect("valid");
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
    #[case::review("refs/meta/reviews/7", Some(Namespace::Review))]
    #[case::pin("refs/meta/pins/reviews/7", Some(Namespace::Pin))]
    #[case::effect("refs/meta/effects/unit", Some(Namespace::Effect))]
    #[case::result("refs/meta/results/unit/abc123", Some(Namespace::Result))]
    #[case::self_run("refs/meta/self/jdc/unit/abc123", Some(Namespace::SelfRun))]
    #[case::toolchain("refs/meta/toolchains/rust-stable", Some(Namespace::Toolchain))]
    #[case::redaction("refs/meta/redactions/abc", Some(Namespace::Redaction))]
    #[case::inbox("refs/meta/inbox/jdc/issue-42", Some(Namespace::Inbox))]
    #[case::account("refs/meta/account", Some(Namespace::Account))]
    #[case::config("refs/meta/config", Some(Namespace::Config))]
    #[case::outside_meta("refs/heads/main", None)]
    #[case::unrecognized("refs/meta/index/abc", Some(Namespace::Unknown))]
    #[case::novel_namespace("refs/meta/widgets/7", Some(Namespace::Unknown))]
    // @relation(meta-ref.namespace, meta-ref.granularity, scope=function, role=Verifies)
    fn classify_matches_the_namespace_table(
        #[case] refname: &str,
        #[case] expected: Option<Namespace>,
    ) {
        assert_eq!(classify(name(refname).as_ref()), expected);
    }

    #[rstest]
    #[case::inbox_entity("refs/meta/inbox/jdc/issue-42", true)]
    #[case::unscoped_inbox_is_still_the_namespace("refs/meta/inbox/legacy", true)]
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
    #[case::scoped("refs/meta/inbox/jdc/issue-42", Some("jdc"))]
    #[case::deep_id("refs/meta/inbox/worker-1/a/b", Some("worker-1"))]
    #[case::unscoped_legacy("refs/meta/inbox/issue-42", None)]
    #[case::self_run("refs/meta/self/jdc/unit/abc", None)]
    #[case::outside_meta("refs/heads/main", None)]
    // @relation(meta-ref.inbox, scope=function, role=Verifies)
    fn inbox_owner_extracts_only_the_member_segment(
        #[case] refname: &str,
        #[case] expected: Option<&str>,
    ) {
        assert_eq!(
            inbox_owner(name(refname).as_ref()),
            expected.map(MemberId::new)
        );
    }

    #[rstest]
    // @relation(meta-ref.namespace, effect.definition, scope=function, role=Verifies)
    fn every_builder_stays_under_refs_meta() {
        let id = MemberId::new("jdc");
        let built = [
            member_ref(&id).expect("valid"),
            issue_ref("42").expect("valid"),
            comment_ref("abc").expect("valid"),
            review_ref("deadbeef", &id).expect("valid"),
            review_pin_ref("deadbeef", &id).expect("valid"),
            effect_ref("unit").expect("valid"),
            result_ref("unit", "abc123").expect("valid"),
            self_result_ref(&id, "unit", "abc123").expect("valid"),
            inbox_ref(&id, "issue-42").expect("valid"),
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
