//! The error type every `ents-model` operation returns.

/// Everything that can go wrong building or parsing `ents-model` values.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A namespace builder (`namespace::member_ref` and friends) composed a
    /// refname that gitoxide's own refname validation rejects — for example,
    /// an id containing `..` or a disallowed control character. The caller
    /// should reject the offending id before offering it to a namespace
    /// builder.
    #[error("invalid refname {name:?}: {source}")]
    InvalidRefName {
        /// The composed refname that failed validation.
        name: String,
        /// gitoxide's own refname validation error.
        #[source]
        source: gix::validate::reference::name::Error,
    },

    /// A value handed to one of this crate's `FromStr` implementations
    /// (for example, [`crate::claim::Verdict`]'s kebab-case parse) did not
    /// match any known form.
    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    /// A [`crate::Claim`]'s binding could not be serialized into or
    /// deserialized from the object store
    /// ([`crate::claim::Claim::new`], [`crate::claim::Claim::binding`]).
    #[error("binding operation failed: {0}")]
    Anchor(#[from] ents_anchor::Error),
}

/// The `Result` alias every `ents-model` operation returns.
pub type Result<T> = std::result::Result<T, Error>;
