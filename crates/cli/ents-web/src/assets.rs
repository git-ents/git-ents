//! Static assets embedded at compile time so the built binary stays
//! self-contained -- no runtime fetch, no separate asset bundle to ship
//! alongside `git-ents`. `ents.css` is the hand-rolled pre-redo stylesheet
//! (`pre-redo:crates/git-ents-server/src/web/style.css`), ported rather
//! than vendored. [`FONTS_HREF`] is this crate's one exception: the
//! pre-redo brand type stack is only available from Google Fonts, so it is
//! loaded at request time rather than embedded.

pub(crate) const OVERRIDES: &str = include_str!("assets/ents.css");

/// Google Fonts stylesheet URL for the pre-redo brand type stack (DM Sans,
/// IBM Plex Mono, Lora) -- mirrors
/// `pre-redo:crates/git-ents-server/src/web/assets.rs`'s `FONTS` const.
pub(crate) const FONTS_HREF: &str = "https://fonts.googleapis.com/css2?family=DM+Sans:wght@400;500;600;700&family=IBM+Plex+Mono:wght@400;500;600&family=Lora:wght@500;600;700&display=swap";
