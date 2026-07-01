//! Inline icons, vendored from [Octicons](https://primer.style/octicons/) (MIT;
//! see `icons/LICENSE`). The upstream `.svg` files are bundled verbatim at
//! compile time and given the page's own `.icon` class as they are emitted, so
//! the UI carries no runtime asset dependency and the icon artwork has a clear,
//! auditable provenance.

use std::sync::LazyLock;

use maud::{Markup, PreEscaped};

/// Adapt an upstream Octicon to this UI: tag it with the `.icon` class the
/// stylesheet targets and mark it decorative for assistive tech. Every vendored
/// file opens with a bare `<svg …>` element, so a single prefix swap suffices.
fn inline(svg: &str) -> String {
    svg.replacen("<svg ", "<svg class=\"icon\" aria-hidden=\"true\" ", 1)
}

/// Define an icon accessor per vendored Octicon file. Each prepares its inline
/// markup once and hands out a cheap clone on use.
macro_rules! icons {
    ($($name:ident => $file:literal),* $(,)?) => {
        $(
            pub(super) fn $name() -> Markup {
                static HTML: LazyLock<String> =
                    LazyLock::new(|| inline(include_str!(concat!("icons/", $file, ".svg"))));
                PreEscaped(HTML.clone())
            }
        )*
    };
}

icons! {
    icon_repo => "repo",
    icon_folder => "file-directory-fill",
    icon_file => "file",
    icon_plus => "plus",
    icon_issue => "issue-opened",
    icon_check => "check",
    icon_chevron => "chevron-right",
    icon_branch => "git-branch",
    icon_tag => "tag",
    icon_clock => "clock",
    icon_commit => "git-commit",
    icon_logo => "north-star",
    icon_search => "search",
}

/// Zero-size icon bundle so Askama templates can emit an icon as
/// `{{ icons.icon_search()|safe }}` — the same inline SVG the free functions
/// hand to Maud, as a raw HTML string (Askama's `safe` filter needs `Display`,
/// which Maud's `PreEscaped` does not implement). Methods are added here as
/// tabs migrate off Maud.
pub(super) struct Icons;

impl Icons {
    pub(super) fn icon_plus(&self) -> String {
        icon_plus().into_string()
    }
    pub(super) fn icon_issue(&self) -> String {
        icon_issue().into_string()
    }
    pub(super) fn icon_check(&self) -> String {
        icon_check().into_string()
    }
    pub(super) fn icon_search(&self) -> String {
        icon_search().into_string()
    }
}
