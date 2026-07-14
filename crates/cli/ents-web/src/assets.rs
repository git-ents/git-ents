//! Static assets embedded at compile time so the built binary stays
//! self-contained -- no runtime fetch, no separate asset bundle to ship
//! alongside `git-ents`. `ents.css` is the hand-rolled pre-redo stylesheet
//! (`pre-redo:crates/git-ents-server/src/web/style.css`), ported rather
//! than vendored -- including its type stack, which this crate's own
//! system-font fallback carries rather than the pre-redo Google Fonts load
//! (see `ents.css`'s own header comment). `ents.js` is new to this crate
//! (pre-redo had no client-side script at all): a vanilla,
//! dependency-free progressive enhancement over `crate::pages::files`'s
//! raw-source blob view -- click-to-select a line or range and an inline
//! comment composer -- served alongside `ents.css` the same way, via
//! `crate::router`'s own `GET /ents.js` route.
//!
//! The icon functions below are vendored Octicons (`.gitvendors`, MIT; see
//! `assets/icons/LICENSE`), re-homed here from
//! `pre-redo:crates/git-ents-server/src/web/icons/` for
//! [`crate::pages::files`]'s directory listing and breadcrumbs -- the same
//! `include_str!`-and-tag pattern
//! `pre-redo:crates/git-ents-server/src/web/icons.rs` used. The workbench
//! shell's own chrome (the `.rail` page-family icons, the `.palette`
//! search glass, the `.branch` pill) draws from [`sprite`] instead: one
//! hand-rolled `<symbol>` sprite embedded per page by
//! `crate::pages::layout`, each use site a tiny [`icon_use`] reference
//! rather than a repeated inline SVG.

use std::sync::LazyLock;

use maud::{Markup, PreEscaped};

pub(crate) const OVERRIDES: &str = include_str!("assets/ents.css");

/// The client-side line-selection/comment-composer script
/// [`crate::router`]'s `GET /ents.js` serves -- see this module's own doc.
pub(crate) const SCRIPT: &str = include_str!("assets/ents.js");

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
}

/// The workbench shell's inline `<symbol>` sprite (see this module's own
/// doc) -- embedded once per page, right after `<body>`, so every
/// [`icon_use`] reference on the page resolves against it.
pub(crate) fn sprite() -> Markup {
    PreEscaped(include_str!("assets/sprite.svg").to_owned())
}

/// An `.icon`-classed, decorative `<use>` reference into [`sprite`] --
/// `id` names one of its `<symbol>`s (`i-home`, `i-files`, ...). Sized
/// entirely by the use site's own CSS rule (`.rail a .icon`,
/// `.palette .icon`, `.branch .icon`), since a symbol carries only a
/// viewBox.
pub(crate) fn icon_use(id: &str) -> Markup {
    PreEscaped(format!(
        "<svg class=\"icon\" aria-hidden=\"true\"><use href=\"#{id}\"/></svg>"
    ))
}
