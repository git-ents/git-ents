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
}

/// The `Result` alias every `ents-model` operation returns.
pub type Result<T> = std::result::Result<T, Error>;
