//! AsciiDoc rendering via the [`acdc`](https://github.com/nlopes/acdc) library.
//!
//! AsciiDoc gets the same treatment Markdown does (`crate::markdown`): an
//! `.adoc`/`.asciidoc` blob in [`crate::pages::files`] renders as a
//! formatted document rather than a plain-text listing. Output is the
//! *embedded* fragment (no `<!DOCTYPE>`/`<html>` frame) so it can drop
//! straight into a `.doc-body`-styled card
//! (`crate::assets::OVERRIDES`).
//!
//! `acdc-converters-core` and `acdc-converters-html` are not on crates.io
//! yet, so they are pinned as git dependencies on the same revision
//! `pre-redo:Cargo.toml` pinned (see this crate's own `Cargo.toml`).
#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "wired into crate::pages::files's blob view in the next commit"
    )
)]

use acdc_converters_core::{Converter, Options as ConvertOptions, inlines_to_string};
use acdc_converters_html::{Processor, RenderOptions};
use acdc_parser::Options as ParseOptions;
use maud::{Markup, PreEscaped, html};

use crate::error::{Error, Result};

/// File extensions that name an AsciiDoc document.
const EXTENSIONS: [&str; 4] = ["adoc", "asciidoc", "asc", "adc"];

/// Whether `name` looks like an AsciiDoc file by its extension.
#[must_use]
pub(crate) fn is_asciidoc(name: &str) -> bool {
    name.rsplit_once('.')
        .is_some_and(|(_, ext)| EXTENSIONS.iter().any(|e| ext.eq_ignore_ascii_case(e)))
}

/// Render AsciiDoc `source` to an embedded HTML fragment. The fragment
/// carries no document frame, so callers place it inside their own
/// container (`.doc-body`).
///
/// `acdc`'s embedded render mode omits both the document frame *and* the
/// visible doctitle/subtitle, so this reconstructs them from the parsed
/// header and prepends them to the embedded body -- carried over from
/// `pre-redo:crates/git-ents-server/src/asciidoc.rs`'s own `to_html`,
/// which hit the same gap: without this, a README's own `= Title` line
/// would silently vanish from the rendered page.
///
/// No sanitization is applied beyond what `acdc`'s HTML converter itself
/// guarantees -- the pre-redo version did the same, emitting the
/// converter's output unescaped via `maud::PreEscaped`.
///
/// # Errors
///
/// [`Error::Asciidoc`] if `source` cannot be parsed or converted.
pub(crate) fn to_html(source: &str) -> Result<Markup> {
    let parsed = acdc_parser::parse(source, &ParseOptions::default())
        .map_err(|err| Error::Asciidoc(err.to_string()))?;
    let doc = parsed.document();

    let heading = doc
        .header
        .as_ref()
        .filter(|h| !h.title.is_empty())
        .map(|h| {
            let title = inlines_to_string(&h.title);
            let subtitle = h.subtitle.as_ref().map(|s| inlines_to_string(s));
            html! {
                h1 { (title) }
                @if let Some(subtitle) = subtitle {
                    p.doc-subtitle { (subtitle) }
                }
            }
        });

    let processor = Processor::new(ConvertOptions::default(), doc.attributes.clone());
    let options = RenderOptions {
        embedded: true,
        ..RenderOptions::default()
    };
    let body = processor
        .convert_to_string(doc, &options)
        .map_err(|err| Error::Asciidoc(err.to_string()))?;
    Ok(match heading {
        Some(heading) => html! { (heading) (PreEscaped(body)) },
        None => PreEscaped(body),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "unit test")]

    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::adoc("readme.adoc", true)]
    #[case::asciidoc("readme.asciidoc", true)]
    #[case::asc("notes.asc", true)]
    #[case::upper("README.ADOC", true)]
    #[case::md("readme.md", false)]
    #[case::no_ext("readme", false)]
    fn is_asciidoc_matches_by_extension(#[case] name: &str, #[case] expected: bool) {
        assert_eq!(is_asciidoc(name), expected);
    }

    #[test]
    fn to_html_reconstructs_the_doctitle_and_renders_a_paragraph() {
        let rendered = to_html("= Title\n\nA paragraph.\n")
            .expect("valid asciidoc")
            .into_string();
        assert!(rendered.contains("<h1>Title</h1>"));
        assert!(rendered.contains("A paragraph."));
    }

    #[test]
    fn to_html_reconstructs_a_subtitle() {
        let rendered = to_html("= Title: Subtitle\n\nBody.\n")
            .expect("valid asciidoc")
            .into_string();
        assert!(rendered.contains("<h1>Title</h1>"));
        assert!(rendered.contains(r#"class="doc-subtitle""#));
        assert!(rendered.contains("Subtitle"));
    }
}
