//! `ents-web`'s error type: every failure a page handler can hit, rendered
//! as an HTTP response by rendered per-page (via the `IntoResponse` impl below) rather than at the
//! type itself — a web frontend renders failures as pages/status codes, not
//! terminal text, so this module stays data-only (mirrors `git-ents`'s own
//! `error.rs` shape, one variant per failure source).

/// Every way a page handler in this crate can fail.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The named entity does not exist.
    #[error("not found: {what}")]
    NotFound {
        /// What was being looked up.
        what: String,
    },

    /// A malformed request: a bad line-range, an unparsable object id, a
    /// missing required form field.
    #[error("invalid request: {0}")]
    InvalidArgument(String),

    /// The gate refused the proposed mutation (`gate.verdict-reason`).
    #[error("rejected: {0}")]
    Refused(String),

    /// `receive` rejected the batch as a stale compare-and-swap.
    #[error("rejected: {name} changed concurrently, retry")]
    Stale {
        /// The ref whose precondition was stale.
        name: String,
    },

    /// A previously redacted object would have been refilled by this
    /// mutation (`receive.redaction-ingest`).
    #[error("refused: object {oid} was redacted and cannot be refilled")]
    Redacted {
        /// The redacted object id.
        oid: gix_hash::ObjectId,
    },

    /// The request's CSRF token was missing or did not match the session's
    /// (`roots.web-session`).
    #[error("invalid or missing CSRF token")]
    BadCsrf,

    /// No session cookie was presented, or it named a session this server
    /// no longer holds in memory (`roots.web-session`): the process
    /// restarted, or the cookie is forged.
    #[error("no valid session")]
    NoSession,

    /// A `gix-ref-store` failure: reading or writing a ref.
    #[error(transparent)]
    Refs(#[from] gix_ref_store::Error),

    /// An `ents-model` failure: building or validating a refname or typed
    /// tree.
    #[error(transparent)]
    Model(#[from] ents_model::Error),

    /// A `facet-git-tree` (de)serialization failure.
    #[error(transparent)]
    Tree(#[from] facet_git_tree::Error),

    /// An `ents-anchor` failure: capturing or projecting a code anchor.
    #[error(transparent)]
    Anchor(#[from] ents_anchor::Error),

    /// An `ents-forge` failure: anchoring, serializing, or proposing a
    /// comment mutation. Boxed: `ents_forge::Error` is large enough on its
    /// own to trip `clippy::result_large_err` if stored inline (mirrors
    /// `git-ents::error::Error::Forge`'s identical boxing).
    #[error(transparent)]
    Forge(Box<ents_forge::Error>),

    /// An `ents-effect` failure: toolchain resolution or import (the error
    /// type `ents-kiln`'s own toolchain module reuses as-is, per that
    /// crate's own doc). Boxed; see [`Error::Forge`]'s own doc.
    #[error(transparent)]
    Effect(Box<ents_effect::Error>),

    /// An `ents-receive` failure: `receive` itself could not reach an
    /// outcome. Boxed; see [`Error::Forge`]'s own doc.
    #[error(transparent)]
    Receive(Box<ents_receive::Error>),

    /// `crate::asciidoc::to_html` could not parse or convert an AsciiDoc
    /// blob (`acdc` reported no more specific error than "could not
    /// convert").
    #[error("could not render asciidoc: {0}")]
    Asciidoc(String),

    /// `crate::pages::files` could not open the served repository or read
    /// its `HEAD` tree/a tree or blob within it (`gix::open`, a tree
    /// lookup, or a blob read).
    #[error("could not read repository: {0}")]
    Repo(String),
}

impl From<ents_forge::Error> for Error {
    fn from(source: ents_forge::Error) -> Self {
        Self::Forge(Box::new(source))
    }
}

impl From<ents_effect::Error> for Error {
    fn from(source: ents_effect::Error) -> Self {
        Self::Effect(Box::new(source))
    }
}

impl From<ents_receive::Error> for Error {
    fn from(source: ents_receive::Error) -> Self {
        Self::Receive(Box::new(source))
    }
}

/// Translate a reached [`ents_receive::Outcome`] into `Ok(())` on success or
/// an [`Error`] otherwise — this crate's counterpart to
/// `git_ents::mutate::outcome_to_result`, kept as a free function here for
/// exactly the same reason: every page that proposes a mutation renders a
/// refusal identically.
///
/// # Errors
///
/// [`Error::Refused`], [`Error::Stale`], or [`Error::Redacted`]; see
/// `git_ents::mutate::outcome_to_result` for the identical mapping this
/// mirrors.
pub fn outcome_to_result(outcome: ents_receive::Outcome) -> Result<()> {
    match outcome.result {
        ents_receive::TxResult::Applied => Ok(()),
        ents_receive::TxResult::Refused => {
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
        ents_receive::TxResult::Rejected { name } => Err(Error::Stale {
            name: name.as_bstr().to_string(),
        }),
        ents_receive::TxResult::Redacted { oid } => Err(Error::Redacted { oid }),
    }
}

/// This crate's `Result` alias.
pub type Result<T> = std::result::Result<T, Error>;

impl axum::response::IntoResponse for Error {
    fn into_response(self) -> axum::response::Response {
        use axum::http::StatusCode;

        let status = match &self {
            Error::NotFound { .. } => StatusCode::NOT_FOUND,
            // A forge entity with no ref at all is as much a 404 as this
            // crate's own NotFound -- the box exists for variant-size
            // hygiene, not to demote the status to a 500.
            Error::Forge(inner) if matches!(inner.as_ref(), ents_forge::Error::NotFound { .. }) => {
                StatusCode::NOT_FOUND
            }
            Error::InvalidArgument(_) | Error::BadCsrf => StatusCode::BAD_REQUEST,
            Error::NoSession => StatusCode::UNAUTHORIZED,
            Error::Refused(_) | Error::Stale { .. } | Error::Redacted { .. } => {
                StatusCode::CONFLICT
            }
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, self.to_string()).into_response()
    }
}
