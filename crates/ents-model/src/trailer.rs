//! Reserved commit trailers (`meta-ref.trailers`).
//!
//! Ref-level metadata that is not entity content lives in the mutation
//! commit's trailers, never inside the tree (`meta-ref.typed-tree`) — a
//! comment's author and timestamp are the running example
//! (`model.comment`). Two trailers are reserved: `Schema-Version:`, for
//! explicit encoding detection if it is ever needed, and `Advance-ref:`, which
//! `ents-gate` (phase 3) compares against the refname actually being
//! updated to bind a signature to its placement.
//!
//! Parsing rides on `gix_object`'s own trailer scanner
//! (`CommitRef::message_trailers`, `git-interpret-trailers`-compatible)
//! rather than re-implementing trailer-block detection here.

use gix::refs::FullName;
use gix_object::commit::MessageRef;

/// The reserved trailer key binding a mutation commit to the refname it was
/// authored for.
pub const ADVANCE_REF: &str = "Advance-ref";

/// The reserved trailer key for explicit encoding detection.
pub const SCHEMA_VERSION: &str = "Schema-Version";

/// The two reserved trailers read from (or written to) a mutation commit's
/// message, per `meta-ref.trailers`.
///
/// A malformed `Advance-ref:` value — one that fails gitoxide's own refname
/// validation — parses as absent rather than as an error: a commit message
/// is untrusted input, and rejecting a bad binding is `ents-gate`'s job
/// (`gate.tip-signed`), not this type's.
// @relation(meta-ref.trailers, scope=file)
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Trailers {
    /// The refname the commit was authored for, if the `Advance-ref:` trailer
    /// is present and well-formed.
    pub ents_ref: Option<FullName>,
    /// The raw `Schema-Version:` value, if present.
    pub schema_version: Option<String>,
}

impl Trailers {
    /// Parse the reserved trailers out of a raw commit message.
    ///
    /// # Examples
    ///
    /// ```
    /// use ents_model::trailer::Trailers;
    ///
    /// let message = b"Enroll jdc\n\nAdvance-ref: refs/meta/member/jdc\n";
    /// let trailers = Trailers::parse(message);
    /// assert_eq!(trailers.ents_ref.expect("present").as_bstr(), "refs/meta/member/jdc");
    /// ```
    #[must_use]
    pub fn parse(message: &[u8]) -> Self {
        let Some(body) = MessageRef::from_bytes(message).body() else {
            return Self::default();
        };

        let mut trailers = Self::default();
        for trailer in body.trailers() {
            if trailer.token.eq_ignore_ascii_case(ADVANCE_REF.as_bytes()) {
                if let Ok(name) = FullName::try_from(trailer.value.to_string()) {
                    trailers.ents_ref = Some(name);
                }
            } else if trailer
                .token
                .eq_ignore_ascii_case(SCHEMA_VERSION.as_bytes())
            {
                trailers.schema_version = Some(trailer.value.to_string());
            }
        }
        trailers
    }

    /// Render the reserved trailers as a `Key: Value\n` block, suitable for
    /// appending to a commit message body. Absent fields contribute no
    /// line; an entirely-empty `Trailers` renders as the empty string.
    ///
    /// # Examples
    ///
    /// ```
    /// use ents_model::trailer::Trailers;
    ///
    /// let name: gix::refs::FullName = "refs/meta/member/jdc".try_into().expect("valid");
    /// let trailers = Trailers {
    ///     ents_ref: Some(name),
    ///     schema_version: None,
    /// };
    /// assert_eq!(trailers.render(), "Advance-ref: refs/meta/member/jdc\n");
    /// ```
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        if let Some(name) = &self.ents_ref {
            out.push_str(ADVANCE_REF);
            out.push_str(": ");
            out.push_str(&name.as_bstr().to_string());
            out.push('\n');
        }
        if let Some(version) = &self.schema_version {
            out.push_str(SCHEMA_VERSION);
            out.push_str(": ");
            out.push_str(version);
            out.push('\n');
        }
        out
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "unit test")]

    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::both(
        b"Subject\n\nBody line.\n\nAdvance-ref: refs/meta/member/jdc\nSchema-Version: 1\n",
        Some("refs/meta/member/jdc"),
        Some("1")
    )]
    #[case::ents_ref_only(
        b"Subject\n\nAdvance-ref: refs/meta/issues/42\n",
        Some("refs/meta/issues/42"),
        None
    )]
    #[case::neither(b"Subject\n\nJust a body, no trailers.\n", None, None)]
    #[case::case_insensitive_key(
        b"Subject\n\nents-ref: refs/meta/comments/1\n",
        Some("refs/meta/comments/1"),
        None
    )]
    #[case::malformed_ref_is_absent(b"Subject\n\nAdvance-ref: not a refname\n", None, None)]
    // @relation(meta-ref.trailers, scope=function, role=Verifies)
    fn parse_reads_reserved_trailers_only(
        #[case] message: &[u8],
        #[case] expected_ref: Option<&str>,
        #[case] expected_version: Option<&str>,
    ) {
        let trailers = Trailers::parse(message);
        assert_eq!(
            trailers.ents_ref.map(|n| n.as_bstr().to_string()),
            expected_ref.map(str::to_owned)
        );
        assert_eq!(trailers.schema_version, expected_version.map(str::to_owned));
    }

    #[rstest]
    // @relation(meta-ref.trailers, scope=function, role=Verifies)
    fn render_then_parse_round_trips() {
        let name: FullName = "refs/meta/effects/unit".try_into().expect("valid");
        let trailers = Trailers {
            ents_ref: Some(name),
            schema_version: Some("1".to_owned()),
        };
        let message = format!("Subject\n\nBody.\n\n{}", trailers.render());
        let parsed = Trailers::parse(message.as_bytes());
        assert_eq!(parsed, trailers);
    }
}
