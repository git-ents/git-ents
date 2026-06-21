//! AsciiDoc rendering via the [`acdc`](https://github.com/nlopes/acdc) library.
//!
//! The forge treats AsciiDoc as its prose format: a repository's `README.adoc`
//! becomes the editorial centerpiece of the overview, and `.adoc`/`.asciidoc`
//! blobs render as formatted documents rather than highlighted source. Output is
//! the *embedded* fragment (no `<!DOCTYPE>`/`<html>` frame) so it can drop
//! straight into a card body styled by the page's own stylesheet.

use acdc_converters_core::{Converter, Options as ConvertOptions};
use acdc_converters_html::{Processor, RenderOptions};
use acdc_parser::Options as ParseOptions;

/// File extensions that name an AsciiDoc document.
const EXTENSIONS: [&str; 4] = ["adoc", "asciidoc", "asc", "adc"];

/// Whether `name` looks like an AsciiDoc file by its extension.
pub(crate) fn is_asciidoc(name: &str) -> bool {
    name.rsplit_once('.')
        .is_some_and(|(_, ext)| EXTENSIONS.iter().any(|e| ext.eq_ignore_ascii_case(e)))
}

/// Render AsciiDoc `source` to an embedded HTML fragment, or `None` if it cannot
/// be parsed or converted. The fragment carries no document frame, so callers
/// place it inside their own container.
pub(crate) fn to_html(source: &str) -> Option<String> {
    let parsed = acdc_parser::parse(source, &ParseOptions::default()).ok()?;
    let doc = parsed.document();
    let processor = Processor::new(ConvertOptions::default(), doc.attributes.clone());
    let options = RenderOptions {
        embedded: true,
        ..RenderOptions::default()
    };
    processor.convert_to_string(doc, &options).ok()
}
