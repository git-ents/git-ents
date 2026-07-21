//! Static assets embedded at compile time so the built binary stays
//! self-contained -- no runtime fetch, no separate asset bundle to ship
//! alongside `git-ents`. `ents.css` is the hand-rolled workbench
//! stylesheet, keyed to the design handoff's tokens and component specs;
//! its `@font-face` rules load the [`FONTS`] IBM Plex faces this crate
//! serves itself (`GET /fonts/{name}`) rather than fetching them from
//! Google Fonts, so the design's exact type ships without a network
//! dependency. `ents.js` is new to this crate
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
//! search glass, the `.branch` pill, the `.editor-open` pill's `↗` mark)
//! draws from [`sprite`] instead: one hand-rolled `<symbol>` sprite
//! embedded per page by `crate::pages::layout`, each use site a tiny
//! [`icon_use`] reference rather than a repeated inline SVG. Every sprite
//! symbol is an original drawing in the design handoff's 24×24 stroke style
//! (`stroke-width: 1.7`, round caps), never a vendored asset, so no
//! third-party icon license applies to the sprite; the Octicons under
//! `assets/icons/` remain this module's only third-party assets.

use std::sync::LazyLock;

use maud::{Markup, PreEscaped};

pub(crate) const OVERRIDES: &str = include_str!("assets/ents.css");

/// The client-side line-selection/comment-composer script
/// [`crate::router`]'s `GET /ents.js` serves -- see this module's own doc.
pub(crate) const SCRIPT: &str = include_str!("assets/ents.js");

/// The self-hosted IBM Plex webfonts the workbench renders in
/// (`assets/fonts/`, SIL OFL, see `assets/fonts/LICENSE`), embedded so the
/// design's exact type is served without a runtime Google Fonts fetch --
/// the same "self-contained binary, no network dependency" rule
/// [`OVERRIDES`] and the vendored Octicons already hold. `ents.css`'s
/// `@font-face` rules name each by its `GET /fonts/{name}` URL; [`font`]
/// resolves that name back to these bytes.
pub(crate) const FONTS: &[(&str, &[u8])] = &[
    (
        "plex-sans-400.woff2",
        include_bytes!("assets/fonts/plex-sans-400.woff2"),
    ),
    (
        "plex-sans-500.woff2",
        include_bytes!("assets/fonts/plex-sans-500.woff2"),
    ),
    (
        "plex-sans-600.woff2",
        include_bytes!("assets/fonts/plex-sans-600.woff2"),
    ),
    (
        "plex-sans-700.woff2",
        include_bytes!("assets/fonts/plex-sans-700.woff2"),
    ),
    (
        "plex-mono-400.woff2",
        include_bytes!("assets/fonts/plex-mono-400.woff2"),
    ),
    (
        "plex-mono-500.woff2",
        include_bytes!("assets/fonts/plex-mono-500.woff2"),
    ),
    (
        "plex-mono-600.woff2",
        include_bytes!("assets/fonts/plex-mono-600.woff2"),
    ),
];

/// The embedded woff2 bytes for `name`, or `None` for a name no
/// `@font-face` rule references -- [`crate::router`]'s `GET /fonts/{name}`
/// handler serves the hit and 404s the miss, so an unknown path can never
/// read outside this fixed table.
pub(crate) fn font(name: &str) -> Option<&'static [u8]> {
    FONTS
        .iter()
        .find(|(file, _)| *file == name)
        .map(|(_, bytes)| *bytes)
}

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
