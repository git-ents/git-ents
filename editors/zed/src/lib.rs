//! Zed extension registering `ents-lsp`, the `git ents lsp` language server.
//!
//! The extension does no downloading and bundles no binary: `git ents lsp`
//! is resolved from `$PATH`, exactly as `git-ents` itself is a `git`
//! subcommand resolved from `$PATH`. See `docs/spec/lens.adoc` (`lens.serve`)
//! for the contract this server implements.

use zed_extension_api::{self as zed, LanguageServerId, Result};

struct EntsExtension;

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
            .which("git")
            .ok_or_else(|| "git not found on $PATH".to_string())?;

        Ok(zed::Command {
            command,
            args: vec!["ents".into(), "lsp".into()],
            env: Default::default(),
        })
    }
}

zed::register_extension!(EntsExtension);
