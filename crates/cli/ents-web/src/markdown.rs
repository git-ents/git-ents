//! Markdown rendering via [`pulldown_cmark`].
//!
//! Markdown gets the same treatment AsciiDoc does (`crate::asciidoc`): a
//! `.md` blob in [`crate::pages::files`] renders as a formatted document
//! rather than a plain-text listing. Output is an embedded fragment (no
//! document frame) styled by [`crate::assets::OVERRIDES`]'s own
//! `.doc-body` rules.
//!
//! A document opening with YAML (`---`) or TOML (`+++`) frontmatter has
//! it stripped from the rendered body ([`split_frontmatter`]) and
//! rendered above the document as a key-value properties table instead
//! ([`crate::render::properties_table`]) -- the same table
//! [`crate::asciidoc`] renders a header's attribute entries through. The
//! parse is deliberately a minimal, line-based one over top-level scalar
//! keys (see [`split_frontmatter`]'s own doc): no YAML/TOML dependency
//! carries its weight for a display-only table that renders anything
//! nested as raw text anyway.

use maud::{Markup, PreEscaped, html as maud_html};
use pulldown_cmark::{Options, Parser, html};

/// File extensions that name a Markdown document.
const EXTENSIONS: [&str; 4] = ["md", "markdown", "mdown", "mkd"];

/// Whether `name` looks like a Markdown file by its extension.
#[must_use]
pub(crate) fn is_markdown(name: &str) -> bool {
    name.rsplit_once('.')
        .is_some_and(|(_, ext)| EXTENSIONS.iter().any(|e| ext.eq_ignore_ascii_case(e)))
}

/// Render Markdown `source` to an embedded HTML fragment, with the tables,
/// footnotes, strikethrough, and task-list extensions people expect from
/// forge-flavored Markdown. Leading YAML/TOML frontmatter is stripped from
/// the body and rendered above it as a properties table (this module's own
/// doc; [`split_frontmatter`]).
///
/// No additional sanitization is applied beyond what `pulldown_cmark`
/// itself guarantees (well-formed HTML output from the parsed Markdown
/// tree, not sanitized against embedded raw HTML in the source) --
/// `pre-redo:crates/git-ents-server/src/markdown.rs`'s own `to_html` did
/// the same: it emitted `pulldown_cmark::html::push_html`'s output
/// unescaped, straight into the page.
#[must_use]
pub(crate) fn to_html(source: &str) -> Markup {
    let (frontmatter, body) = split_frontmatter(source);
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS;
    let mut out = String::new();
    html::push_html(&mut out, Parser::new_ext(body, options));
    maud_html! {
        (crate::render::properties_table(&frontmatter))
        (PreEscaped(out))
    }
}

/// Split leading frontmatter off `source`: `(entries, body)`, where
/// `entries` is empty when `source` carries no frontmatter at all and
/// `body` is the document with the frontmatter block (fences included)
/// stripped.
///
/// Frontmatter is recognized only in the one shape static-site tooling
/// actually writes: the document's very first line is exactly a `---`
/// (YAML) or `+++` (TOML) fence, closed by a later line that is exactly
/// the same fence. A `---` further down the document is a thematic break,
/// never frontmatter, and an unclosed fence is not frontmatter either --
/// both render as ordinary Markdown, untouched.
///
/// The parse between the fences is deliberately minimal and line-based
/// (this module's own doc): an unindented `key: value` (YAML) or
/// `key = value` (TOML) line becomes one entry, with one level of
/// matching surrounding quotes stripped from the value. Anything deeper
/// -- an indented nested block, a list continuation, a TOML `[table]`
/// header and everything after it -- is not parsed: it is appended
/// verbatim, raw text, to the entry it follows
/// ([`crate::render::properties_table`] renders it as-is). A nested block
/// with no preceding entry at all opens one keyed by its own raw first
/// line, so no frontmatter line is ever silently dropped.
pub(crate) fn split_frontmatter(source: &str) -> (Vec<(String, String)>, &str) {
    let (fence, separator) = if source.starts_with("---\n") || source.starts_with("---\r\n") {
        ("---", ':')
    } else if source.starts_with("+++\n") || source.starts_with("+++\r\n") {
        ("+++", '=')
    } else {
        return (Vec::new(), source);
    };

    // Walk physical lines by byte offset so the body can be returned as a
    // slice of `source` rather than a rebuilt copy.
    let mut offset: usize = 0;
    let mut lines = Vec::new();
    let mut close = None;
    for line in source.split_inclusive('\n') {
        let text = line.trim_end_matches(['\n', '\r']);
        if offset > 0 && text == fence {
            close = Some(offset.saturating_add(line.len()));
            break;
        }
        if offset > 0 {
            lines.push(text);
        }
        offset = offset.saturating_add(line.len());
    }
    let Some(body_start) = close else {
        return (Vec::new(), source);
    };

    let mut entries: Vec<(String, String)> = Vec::new();
    let mut raw_only = false;
    for line in lines {
        let top_level = !raw_only
            && !line.starts_with([' ', '\t'])
            && line
                .split_once(separator)
                .is_some_and(|(key, _)| is_bare_key(key));
        if top_level && let Some((key, value)) = line.split_once(separator) {
            entries.push((key.trim().to_owned(), unquote(value.trim()).to_owned()));
            continue;
        }
        if line.trim().is_empty() {
            continue;
        }
        if separator == '=' && line.trim_start().starts_with('[') {
            // A TOML `[table]` header: nothing after it is top-level, so
            // the rest of the block stays raw under this one entry.
            raw_only = true;
            entries.push((line.trim().to_owned(), String::new()));
            continue;
        }
        match entries.last_mut() {
            Some((_, value)) => {
                if !value.is_empty() {
                    value.push('\n');
                }
                value.push_str(line);
            }
            None => entries.push((line.trim().to_owned(), String::new())),
        }
    }
    (entries, source.get(body_start..).unwrap_or(""))
}

/// Whether `key` looks like a bare frontmatter key: non-empty, no
/// whitespace or quoting inside it -- what keeps [`split_frontmatter`]
/// from misreading prose containing a `:` (or a quoted value containing
/// `=`) as an entry.
fn is_bare_key(key: &str) -> bool {
    let key = key.trim_end();
    !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// Strip one level of matching surrounding single or double quotes from a
/// scalar frontmatter value.
fn unquote(value: &str) -> &str {
    let stripped = value
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .or_else(|| {
            value
                .strip_prefix('\'')
                .and_then(|rest| rest.strip_suffix('\''))
        });
    stripped.unwrap_or(value)
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::md("readme.md", true)]
    #[case::markdown("readme.markdown", true)]
    #[case::upper("README.MD", true)]
    #[case::adoc("readme.adoc", false)]
    #[case::no_ext("readme", false)]
    fn is_markdown_matches_by_extension(#[case] name: &str, #[case] expected: bool) {
        assert_eq!(is_markdown(name), expected);
    }

    #[test]
    fn to_html_renders_a_heading_and_a_table() {
        let rendered = to_html("# Title\n\n| a | b |\n|---|---|\n| 1 | 2 |\n").into_string();
        assert!(rendered.contains("<h1>Title</h1>"));
        assert!(rendered.contains("<table>"));
    }

    #[test]
    fn to_html_strips_frontmatter_from_the_body_and_renders_it_as_properties() {
        let rendered = to_html("---\ntitle: Design Notes\n---\n# Title\n\nBody.\n").into_string();
        assert!(
            rendered.contains("doc-props"),
            "the properties table renders"
        );
        assert!(rendered.contains("Design Notes"));
        assert!(rendered.contains("<h1>Title</h1>"));
        assert!(
            !rendered.contains("<hr"),
            "the fences are stripped, not rendered as thematic breaks: {rendered}"
        );
    }

    #[test]
    fn split_frontmatter_reads_yaml_scalars_and_strips_the_block() {
        let (entries, body) = split_frontmatter("---\ntitle: \"Hello\"\ndraft: true\n---\n# Doc\n");
        assert_eq!(
            entries,
            vec![
                ("title".to_owned(), "Hello".to_owned()),
                ("draft".to_owned(), "true".to_owned()),
            ]
        );
        assert_eq!(body, "# Doc\n");
    }

    #[test]
    fn split_frontmatter_reads_toml_scalars_behind_plus_fences() {
        let (entries, body) = split_frontmatter("+++\ntitle = 'Hi'\nweight = 3\n+++\nBody.\n");
        assert_eq!(
            entries,
            vec![
                ("title".to_owned(), "Hi".to_owned()),
                ("weight".to_owned(), "3".to_owned()),
            ]
        );
        assert_eq!(body, "Body.\n");
    }

    #[test]
    fn split_frontmatter_keeps_a_nested_yaml_block_as_raw_text_under_its_key() {
        let (entries, body) = split_frontmatter("---\ntags:\n  - a\n  - b\nname: x\n---\nBody.\n");
        assert_eq!(
            entries,
            vec![
                ("tags".to_owned(), "  - a\n  - b".to_owned()),
                ("name".to_owned(), "x".to_owned()),
            ]
        );
        assert_eq!(body, "Body.\n");
    }

    #[test]
    fn split_frontmatter_keeps_a_toml_table_and_everything_after_it_raw() {
        let (entries, _body) =
            split_frontmatter("+++\ntitle = 'Hi'\n[params]\nx = 1\n+++\nBody.\n");
        assert_eq!(
            entries,
            vec![
                ("title".to_owned(), "Hi".to_owned()),
                ("[params]".to_owned(), "x = 1".to_owned()),
            ]
        );
    }

    #[rstest]
    #[case::no_fence_at_all("# Just a doc\n")]
    #[case::fence_not_first("\n---\nkey: value\n---\n")]
    #[case::unclosed_fence("---\nkey: value\n# Doc\n")]
    #[case::thematic_break_later("# Doc\n\n---\n\nMore.\n")]
    fn split_frontmatter_leaves_a_document_without_frontmatter_untouched(#[case] source: &str) {
        let (entries, body) = split_frontmatter(source);
        assert!(entries.is_empty());
        assert_eq!(body, source);
    }

    #[test]
    fn split_frontmatter_keeps_a_prose_colon_line_raw_rather_than_splitting_it() {
        let (entries, _body) =
            split_frontmatter("---\nnote this: is prose\nreal-key: yes\n---\nBody.\n");
        assert_eq!(
            entries,
            vec![
                // Not a bare key ("note this" holds a space), so the line
                // stays raw -- and with no entry before it, it opens one
                // keyed by its own text rather than being dropped.
                ("note this: is prose".to_owned(), String::new()),
                ("real-key".to_owned(), "yes".to_owned()),
            ]
        );
    }
}
