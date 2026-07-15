//! `git ents toolchain`'s argument grammar — `figue` derive definitions
//! only.
//!
//! Per this project's engineering conventions, this module carries no
//! logic: every doc comment here becomes `--help` text, and `git-ents`'s
//! own `exe` module is the only place a [`ToolchainAction`] variant is
//! interpreted.

use std::path::PathBuf;

use facet::Facet;
use figue as args;

/// `git ents toolchain` actions.
#[derive(Facet)]
#[repr(u8)]
pub enum ToolchainAction {
    /// List the toolchains currently defined.
    List,
    /// Import a local directory as toolchain `name`, embedding its
    /// contents whole (`ents_kiln::Recipe::Embedded`).
    Import {
        /// Name to record the toolchain under
        /// (`refs/meta/toolchains/<name>`).
        #[facet(args::positional)]
        name: String,
        /// Directory of executables to import, activated on `PATH` when
        /// an effect declares this toolchain.
        #[facet(args::positional)]
        bin: PathBuf,
        /// Key to sign with; defaults to `user.signingkey`.
        #[facet(args::named)]
        key: Option<PathBuf>,
    },
    /// Show a toolchain's provenance.
    View {
        /// Name (`refs/meta/toolchains/<name>`) to view.
        #[facet(args::positional)]
        name: String,
    },
    /// Show a toolchain's import history — the ref's own commit log.
    Log {
        /// Name (`refs/meta/toolchains/<name>`) to show history for.
        #[facet(args::positional)]
        name: String,
    },
}
