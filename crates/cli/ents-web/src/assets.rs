//! Static assets embedded at compile time so the built binary stays
//! self-contained -- no runtime fetch, no separate asset bundle to ship
//! alongside `git-ents`. `ents.css` is the hand-rolled pre-redo stylesheet
//! (`pre-redo:crates/git-ents-server/src/web/style.css`), ported rather
//! than vendored -- including its type stack, which this crate's own
//! system-font fallback carries rather than the pre-redo Google Fonts load
//! (see `ents.css`'s own header comment).
//!
//! The icon functions below are vendored Octicons (`.gitvendors`, MIT; see
//! `assets/icons/LICENSE`), re-homed here from
//! `pre-redo:crates/git-ents-server/src/web/icons/` for
//! [`crate::pages::files`]'s directory listing and breadcrumbs and for the
//! shell chrome [`crate::pages::layout`] draws (the `nav.site-nav` search
//! stub and the `.repo-header` branch pill) -- the same
//! `include_str!`-and-tag pattern
//! `pre-redo:crates/git-ents-server/src/web/icons.rs` used.

use std::sync::LazyLock;

use maud::{Markup, PreEscaped};

pub(crate) const OVERRIDES: &str = include_str!("assets/ents.css");

/// Adapt a vendored Octicon to this UI: tag it with the `.icon` class the
/// stylesheet targets and mark it decorative for assistive tech. Every
/// vendored file opens with a bare `<svg …>` element, so a single prefix
/// swap suffices (mirrors
/// `pre-redo:crates/git-ents-server/src/web/icons.rs`'s own `inline`).
fn inline(svg: &str) -> String {
    svg.replacen("<svg ", "<svg class=\"icon\" aria-hidden=\"true\" ", 1)
}

/// Define an icon accessor per vendored Octicon file. Each prepares its
/// inline markup once and hands out a cheap clone on use.
macro_rules! icons {
    ($($name:ident => $file:literal),* $(,)?) => {
        $(
            pub(crate) fn $name() -> Markup {
                static HTML: LazyLock<String> =
                    LazyLock::new(|| inline(include_str!(concat!("assets/icons/", $file, ".svg"))));
                PreEscaped(HTML.clone())
            }
        )*
    };
}

icons! {
    icon_folder => "file-directory-fill",
    icon_file => "file",
    icon_chevron => "chevron-right",
    icon_search => "search",
    icon_branch => "git-branch",
}
