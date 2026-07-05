//! MIME-keyed document rendering: the one place that decides, from a
//! document's declared or inferred MIME type, which of [`asciidoc`] or
//! [`markdown`]'s converters turns it into HTML (for the web UI) or plain
//! text (for the CLI). A lookup table rather than a trait hierarchy, the
//! same style as `registry::RECIPES` in the `git-ents` CLI — MIME is an open
//! namespace, so unrecognized types fall through to a passthrough instead of
//! refusing to render at all.

use crate::{asciidoc, markdown};

/// The MIME type this crate treats prose documents as when nothing else
/// declares one — e.g. a comment or issue body, which carries no filename to
/// infer an extension from.
pub const DEFAULT_PROSE_MIME: &str = "text/asciidoc";

/// Guess a document's MIME type from its filename's extension. Replaces
/// separate `is_asciidoc`/`is_markdown` extension checks with one lookup
/// that both HTML and text rendering key off of.
pub fn mime_for_name(name: &str) -> &'static str {
    if asciidoc::is_asciidoc(name) {
        "text/asciidoc"
    } else if markdown::is_markdown(name) {
        "text/markdown"
    } else {
        "text/plain"
    }
}

/// Render `source` (declared or inferred as `mime`) to an embedded HTML
/// fragment. Unrecognized MIME types fall through to an escaped `<pre>`
/// block rather than an error.
pub fn to_html(mime: &str, source: &str) -> String {
    match mime {
        "text/asciidoc" => asciidoc::to_html(source),
        "text/markdown" => Some(markdown::to_html(source)),
        _ => None,
    }
    .unwrap_or_else(|| maud::html! { pre { (source) } }.into_string())
}

/// Render `source` (declared or inferred as `mime`) to plain text, for
/// terminal output. Unrecognized MIME types fall through to `source`
/// verbatim.
pub fn to_text(mime: &str, source: &str) -> String {
    match mime {
        "text/asciidoc" => asciidoc::to_text(source),
        _ => None,
    }
    .unwrap_or_else(|| source.to_owned())
}
