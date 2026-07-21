//! Which editor the serving user works in, and deep links into it.
//!
//! The web surface is an escalation from the editor, never a destination
//! of its own (`docs/web-workbench-plan.adoc`), so every code location a
//! page renders carries an "open in editor" affordance pointing back at
//! the desk the reader came from (`crate::pages`'s `editor_open`). The
//! editor is resolved from `ENTS_EDITOR`, then `EDITOR` -- the same
//! override-then-general order git applies to `GIT_EDITOR`/`EDITOR` --
//! once per process ([`detected`]); an absent or unrecognized value
//! renders no affordance at all rather than a dead link.
//!
//! Deep links use each editor's own URL scheme (`zed://file/...`,
//! `vscode://file/...`). Neovim has no scheme of its own, so its links
//! use the community `nvim://file/...` shape -- they work only where the
//! reader has registered a handler for it, which is stated here rather
//! than hidden: the affordance's `title` names the editor the user
//! configured (`crate::pages`'s `editor_open` renders the shared teal
//! `↗` pill for every editor alike).

use std::path::Path;
use std::sync::LazyLock;

/// The editors this crate can deep-link into, resolved by [`detected`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Editor {
    /// `zed://file/<path>:<line>` (Zed's own scheme).
    Zed,
    /// `vscode://file/<path>:<line>` (VS Code's own scheme; Codium
    /// installs it too).
    VsCode,
    /// `nvim://file/<path>:<line>` -- no official scheme exists, so this
    /// is the community handler shape (see this module's own doc).
    Neovim,
}

impl Editor {
    /// The editor's display name, the affordance's `title` text.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Zed => "Zed",
            Self::VsCode => "VS Code",
            Self::Neovim => "Neovim",
        }
    }

    /// The URL-scheme prefix up to and including `file` -- the deep link
    /// is `<scheme>/<absolute path>[:<line>]`.
    fn scheme(self) -> &'static str {
        match self {
            Self::Zed => "zed://file",
            Self::VsCode => "vscode://file",
            Self::Neovim => "nvim://file",
        }
    }

    /// The deep link opening `abs` (an absolute path) in this editor,
    /// at `line` when given.
    pub(crate) fn deep_link(self, abs: &Path, line: Option<u64>) -> String {
        let scheme = self.scheme();
        let path = abs.display();
        match line {
            Some(line) => format!("{scheme}{path}:{line}"),
            None => format!("{scheme}{path}"),
        }
    }
}

/// Parse one editor-variable value: the command's first token's basename,
/// matched against the launchers each recognized editor ships. `None` for
/// anything else -- an unknown editor gets no affordance, never a dead
/// link.
fn parse(value: &str) -> Option<Editor> {
    let command = value.split_whitespace().next()?;
    let name = Path::new(command)
        .file_name()?
        .to_string_lossy()
        .to_lowercase();
    match name.as_str() {
        "zed" | "zeditor" => Some(Editor::Zed),
        "code" | "code-insiders" | "codium" | "vscodium" => Some(Editor::VsCode),
        "nvim" | "neovim" | "neovide" | "vim" | "gvim" => Some(Editor::Neovim),
        _ => None,
    }
}

/// The serving user's editor: the first of `ENTS_EDITOR`, `EDITOR` that
/// names one this crate recognizes ([`parse`]), read once per process --
/// `git ents serve` runs in the user's own environment, so the variables
/// are the same ones their shell hands every other tool.
pub(crate) fn detected() -> Option<Editor> {
    static DETECTED: LazyLock<Option<Editor>> = LazyLock::new(|| {
        ["ENTS_EDITOR", "EDITOR"]
            .iter()
            .filter_map(|name| std::env::var(name).ok())
            .find_map(|value| parse(&value))
    });
    *DETECTED
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::bare_zed("zed", Some(Editor::Zed))]
    #[case::zed_with_flags("zed --wait", Some(Editor::Zed))]
    #[case::absolute_code("/usr/local/bin/code -g", Some(Editor::VsCode))]
    #[case::codium("codium", Some(Editor::VsCode))]
    #[case::nvim("nvim", Some(Editor::Neovim))]
    #[case::vim_maps_to_the_neovim_icon("vim", Some(Editor::Neovim))]
    #[case::neovide("neovide", Some(Editor::Neovim))]
    #[case::unknown("ed", None)]
    #[case::empty("", None)]
    fn parse_matches_the_command_basename(#[case] value: &str, #[case] expected: Option<Editor>) {
        assert_eq!(parse(value), expected);
    }

    #[rstest]
    fn deep_link_carries_scheme_path_and_line() {
        let abs = Path::new("/repo/src/main.rs");
        assert_eq!(
            Editor::Zed.deep_link(abs, Some(21)),
            "zed://file/repo/src/main.rs:21"
        );
        assert_eq!(
            Editor::VsCode.deep_link(abs, None),
            "vscode://file/repo/src/main.rs"
        );
        assert_eq!(
            Editor::Neovim.deep_link(abs, Some(3)),
            "nvim://file/repo/src/main.rs:3"
        );
    }
}
