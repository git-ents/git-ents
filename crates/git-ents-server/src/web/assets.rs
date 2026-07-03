//! Static front-end assets, bundled into the binary at compile time so the UI
//! has no runtime file or framework dependencies.

/// Web fonts, matching the typography of <https://jdc.pub>.
pub(super) const FONTS: &str = "https://fonts.googleapis.com/css2?family=DM+Sans:wght@400;500;600;700&family=IBM+Plex+Mono:wght@400;500;600&family=Lora:wght@500;600;700&display=swap";

/// Hand-written stylesheet (no external CSS framework) so the look stays stable
/// and self-contained. Colors, type, and radii track <https://jdc.pub>, with a
/// `prefers-color-scheme` block for automatic dark mode.
pub(super) const STYLE: &str = include_str!("style.css");

/// Clipboard handler for the clone-URL copy button.
pub(super) const COPY_SCRIPT: &str = include_str!("copy.js");

/// `asciinema-player` v3.17.0 (Apache-2.0), vendored rather than pulled from a
/// CDN so a check recording still plays back with the server fully offline.
/// Only loaded on the check-recording page, not bundled into every page like
/// [`STYLE`]/[`COPY_SCRIPT`] — it is far larger than either.
pub(super) const ASCIINEMA_PLAYER_JS: &str = include_str!("asciinema-player.min.js");

/// Stylesheet for [`ASCIINEMA_PLAYER_JS`].
pub(super) const ASCIINEMA_PLAYER_CSS: &str = include_str!("asciinema-player.css");
