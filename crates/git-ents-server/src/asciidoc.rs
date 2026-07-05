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
///
/// ## Requirements
///
/// @relation(web.render-registry, web.syntax-highlight)
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

/// Render AsciiDoc `source` to plain text via acdc's `cat`-like terminal
/// converter — the same parser [`to_html`] uses, feeding a converter meant
/// for TTY output (a shell, `git ents comment show`) instead of a browser.
pub(crate) fn to_text(source: &str) -> Option<String> {
    let parsed = acdc_parser::parse(source, &ParseOptions::default()).ok()?;
    let doc = parsed.document();
    let processor =
        acdc_converters_terminal::Processor::new(ConvertOptions::default(), doc.attributes.clone());
    let mut output = Vec::new();
    let source = WarningSource::new("terminal");
    let mut warnings = Vec::new();
    let mut diagnostics = Diagnostics::new(&source, &mut warnings);
    processor
        .write_to(doc, &mut output, None, None, &mut diagnostics)
        .ok()?;
    for warning in &warnings {
        eprintln!("asciidoc text render: {warning}");
    }
    String::from_utf8(output).ok()
}

/// CSS for the `.terminal-view` player acdc's HTML converter emits, vendored
/// here because embedded-fragment output carries no `<head>` to link or inline
/// it from (see [`to_html`]'s doctitle note for the same embedded-mode gap).
/// Based on acdc's built-in stylesheet (keep the box/layout rules in sync if it
/// drifts), but `--light`/`--dark` are repainted with the site's own theme
/// variables (`crates/git-ents-server/src/web/style.css`) rather than acdc's
/// fixed hex pair: our synthesized `[terminal]`/`[terminal%replay]` source never
/// carries a `:dark-mode:` attribute, so acdc always picks `--light`, which
/// otherwise renders a fixed light-on-light box no matter the browser's theme.
pub(crate) const TERMINAL_VIEW_CSS: &str = "\
.terminal-view{margin:1.25em 0;max-width:100%;overflow:auto;border-radius:var(--radius-sm);border:1px solid var(--color-border);box-shadow:var(--shadow-sm)}
.terminal-view__screen{margin:0;padding:18px;font:14px/1.45 var(--font-mono);white-space:pre;tab-size:4}
.terminal-view--light,.terminal-view--dark{background-color:var(--color-code-bg);color:var(--color-text)}
.terminal-view__viewport{overflow:auto;max-width:100%;padding:0 18px 18px}
.terminal-view__stream{margin:0;padding:0;width:max-content;font-family:var(--font-mono);font-size:14px;line-height:1.2;white-space:normal;tab-size:4}
.terminal-view__row{white-space:pre;min-height:1.2em}
";

/// Whether an asciicast v2/v3 `recording` decodes to no visible terminal
/// output — e.g. a check that passed without printing anything beyond a
/// trailing newline. acdc's replay player renders this as a bare empty box
/// with no explanation, so callers should check this first and show their
/// own message instead. Checking the decoded bytes rather than the raw JSONL
/// lines matters: an output event carrying just `"\n"` is a non-empty JSON
/// line but has nothing worth showing.
pub(crate) fn recording_has_no_output(recording: &str) -> bool {
    extract_output(recording).trim().is_empty()
}

/// Render the *current* screen of an in-progress asciicast v2 recording as a
/// static terminal snapshot via acdc's plain `[terminal]` block (no replay
/// scrubber — a running check has no fixed timeline yet, just a screen that
/// keeps changing), or `None` if it cannot be parsed or converted. The
/// asciicast recording stays the single source of truth for the check's
/// output; this only reconstitutes the raw bytes acdc's terminal emulator
/// needs; unlike [`render_recording`], it does not go through acdc's asciicast
/// parser, since that produces a scrubbable timeline rather than one snapshot.
pub(crate) fn render_live(recording: &str) -> Option<String> {
    let ansi = extract_output(recording);
    let source = format!("[terminal]\n----\n{ansi}\n----\n");
    let parsed = acdc_parser::parse(&source, &ParseOptions::default()).ok()?;
    let doc = parsed.document();
    let processor = Processor::new(ConvertOptions::default(), doc.attributes.clone());
    let options = RenderOptions {
        embedded: true,
        ..RenderOptions::default()
    };
    let mut output = Vec::new();
    let source = WarningSource::new("html").with_variant("live-recording");
    let mut warnings = Vec::new();
    let mut diagnostics = Diagnostics::new(&source, &mut warnings);
    processor
        .convert_to_writer(doc, &mut output, &options, &mut diagnostics)
        .ok()?;
    for warning in &warnings {
        eprintln!("live check recording render: {warning}");
    }
    String::from_utf8(output).ok()
}

/// Concatenate every `[time, "o", data]` event's `data` field out of an
/// asciicast v2 recording, in order, undoing the JSON escaping the checks
/// worker applies when it writes them — the raw terminal bytes underneath the
/// recording, for feeding to a *static* terminal renderer (see
/// [`render_live`]). The finished-recording path doesn't need this: acdc's own
/// asciicast parser (used by [`render_recording`]) reads the format natively.
fn extract_output(recording: &str) -> String {
    let mut out = String::new();
    for line in recording.lines().skip(1) {
        if let Some(data) = event_data(line) {
            out.push_str(&data);
        }
    }
    out
}

/// Extract and unescape the `data` field of one `[time, "o", "data"]` event
/// line, or `None` if the line does not look like one.
fn event_data(line: &str) -> Option<String> {
    const MARKER: &str = "\"o\", \"";
    let start = line.find(MARKER)?.checked_add(MARKER.len())?;
    let rest = line.get(start..)?;
    let end = rest.rfind("\"]")?;
    Some(unescape_json_string(rest.get(..end)?))
}

/// The inverse of the checks worker's hand-rolled JSON string escaping:
/// unescape `"`, `\`, the recognized single-character escapes, and `\uXXXX`
/// control-code escapes, passing everything else through unchanged.
fn unescape_json_string(escaped: &str) -> String {
    let mut out = String::with_capacity(escaped.len());
    let mut chars = escaped.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('u') => {
                let hex: String = chars.by_ref().take(4).collect();
                if let Some(ch) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    out.push(ch);
                }
            }
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
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
