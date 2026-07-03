//! AsciiDoc rendering via the [`acdc`](https://github.com/nlopes/acdc) library.
//!
//! The forge treats AsciiDoc as its prose format: a repository's `README.adoc`
//! becomes the editorial centerpiece of the overview, and `.adoc`/`.asciidoc`
//! blobs render as formatted documents rather than highlighted source. Output is
//! the *embedded* fragment (no `<!DOCTYPE>`/`<html>` frame) so it can drop
//! straight into a card body styled by the page's own stylesheet.

use acdc_converters_core::{
    Converter, Diagnostics, Options as ConvertOptions, WarningSource, inlines_to_string,
};
use acdc_converters_html::{Processor, RenderOptions};
use acdc_parser::Options as ParseOptions;
use maud::html;

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

    // Embedded mode omits the document frame *and* the visible doctitle, so
    // rebuild the title and subtitle from the parsed header — the README's h1 is
    // the centerpiece of the overview.
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
            .into_string()
        });

    let processor = Processor::new(ConvertOptions::default(), doc.attributes.clone());
    let options = RenderOptions {
        embedded: true,
        ..RenderOptions::default()
    };
    let body = processor.convert_to_string(doc, &options).ok()?;
    Some(match heading {
        Some(heading) => heading + &body,
        None => body,
    })
}

/// CSS for the `.terminal-view` player acdc's HTML converter emits, vendored
/// here because embedded-fragment output carries no `<head>` to link or inline
/// it from (see [`to_html`]'s doctitle note for the same embedded-mode gap).
/// Lifted verbatim from acdc's built-in stylesheet; keep in sync if it drifts.
pub(crate) const TERMINAL_VIEW_CSS: &str = "\
.terminal-view{margin:1.25em 0;max-width:100%;overflow:auto;border-radius:8px;box-shadow:0 16px 50px rgba(0,0,0,.18)}
.terminal-view__screen{margin:0;padding:18px;font:14px/1.45 ui-monospace,SFMono-Regular,\"SF Mono\",Menlo,Consolas,\"Liberation Mono\",monospace;white-space:pre;tab-size:4}
.terminal-view--light{background-color:#f6f8fa;color:#1f2328}
.terminal-view--dark{background-color:#0d1117;color:#e6edf3}
.terminal-view__viewport{overflow:auto;max-width:100%;padding:0 18px 18px}
.terminal-view__stream{margin:0;padding:0;width:max-content;font-family:ui-monospace,SFMono-Regular,\"SF Mono\",Menlo,Consolas,\"Liberation Mono\",monospace;font-size:14px;line-height:1.2;white-space:normal;tab-size:4}
.terminal-view__row{white-space:pre;min-height:1.2em}
";

/// Whether an asciicast v2/v3 `recording` has no output events beyond its
/// header line — e.g. a check that passed without printing anything. acdc's
/// replay player renders this as a bare empty box with no explanation, so
/// callers should check this first and show their own message instead.
pub(crate) fn recording_has_no_output(recording: &str) -> bool {
    recording.lines().skip(1).all(|line| line.trim().is_empty())
}

/// Render an asciicast v2/v3 `recording` as a replayable terminal session via
/// acdc's `[terminal%replay]` block, or `None` if it cannot be parsed or
/// converted. Wraps `recording` in a listing block, so a recording containing
/// a `----` line of its own would break out early; asciicast JSONL never
/// produces that on its own line.
pub(crate) fn render_recording(recording: &str) -> Option<String> {
    let source = format!("[terminal%replay,format=asciicast]\n----\n{recording}\n----\n");
    let parsed = acdc_parser::parse(&source, &ParseOptions::default()).ok()?;
    let doc = parsed.document();
    let processor = Processor::new(ConvertOptions::default(), doc.attributes.clone());
    let options = RenderOptions {
        embedded: true,
        ..RenderOptions::default()
    };
    let mut output = Vec::new();
    let source = WarningSource::new("html").with_variant("recording");
    let mut warnings = Vec::new();
    let mut diagnostics = Diagnostics::new(&source, &mut warnings);
    processor
        .convert_to_writer(doc, &mut output, &options, &mut diagnostics)
        .ok()?;
    for warning in &warnings {
        eprintln!("check recording render: {warning}");
    }
    String::from_utf8(output).ok()
}
