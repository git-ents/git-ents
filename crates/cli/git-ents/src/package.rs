//! The convention `git ents` mounts a package's subcommand through
//! (`docs/abstractions.adoc`, "CLI dispatch seam"): each package owns its
//! own figue action enum, defined in its own crate — never here — and
//! `crate::cli::Top` references that type directly as a variant's field.
//! This trait exists to pin that pairing at compile time, not to
//! register anything at runtime: figue resolves `Top`'s subcommand
//! grammar from its `#[derive(Facet)]` shape alone, so there is no
//! dynamic mounting step to generalize (`git-ents-engineering`: no
//! abstraction beyond what forge and kiln concretely require).
//! Kernel-owned commands (`setup`, `members`, `account`, `effect`,
//! `inbox`, `redact`, `hook`) are not packages under this trait:
//! `git-ents` is their composition root directly, with no package
//! boundary to pin.

/// A `git ents` package mounted into the CLI's subcommand surface: it
/// owns one top-level subcommand's figue action enum, defined in its own
/// crate.
pub trait Package {
    /// This package's subcommand action enum.
    type Action;
}

/// The forge package: owns `git ents comment`.
pub struct Forge;
impl Package for Forge {
    type Action = ents_forge::comment::CommentAction;
}

/// The kiln package: owns `git ents toolchain`.
pub struct Kiln;
impl Package for Kiln {
    type Action = ents_kiln::toolchain::ToolchainAction;
}
