//! Zed extension registering `ents-lsp`, the `git ents lsp` language server.
//!
//! The extension does no downloading and bundles no binary: `git ents lsp`
//! is resolved from `$PATH`, exactly as `git-ents` itself is a `git`
//! subcommand resolved from `$PATH`. See `docs/spec/lens.adoc` (`lens.serve`)
//! for the contract this server implements.

use zed_extension_api::{self as zed, LanguageServerId, Result};

struct EntsExtension;

pub(crate) static BIN_NAME: &str =
    "/Users/joey/Workspace/codes/git/ents/git-ents/target/debug/git-ents";

impl zed::Extension for EntsExtension {
    fn new() -> Self {
        Self
    }

    fn language_server_command(
        &mut self,
        _language_server_id: &LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<zed::Command> {
        let command = worktree
            .which(BIN_NAME)
            .ok_or_else(|| format!("`{}` not found on `$PATH`", BIN_NAME))?;

        Ok(zed::Command {
            command,
            args: vec!["lsp".into()],
            env: Default::default(),
        })
    }
}

zed::register_extension!(EntsExtension);
