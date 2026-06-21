//! Git Ents — helpful guardians of your git trees.

pub mod signers;

/// Returns the tagline describing what the ents do.
///
/// # Examples
///
/// ```
/// assert_eq!(git_ents::tagline(), "Helpful guardians of your git trees.");
/// ```
#[must_use]
pub fn tagline() -> &'static str {
    "Helpful guardians of your git trees."
}
