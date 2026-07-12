//! Markdown rendering via [`pulldown_cmark`].
//!
//! Markdown gets the same treatment AsciiDoc does (`crate::asciidoc`): a
//! `.md` blob in [`crate::pages::files`] renders as a formatted document
//! rather than a plain-text listing. Output is an embedded fragment (no
//! document frame) styled by [`crate::assets::OVERRIDES`]'s own
//! `.doc-body` rules.

use maud::{Markup, PreEscaped};
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
/// forge-flavored Markdown.
///
/// No additional sanitization is applied beyond what `pulldown_cmark`
/// itself guarantees (well-formed HTML output from the parsed Markdown
/// tree, not sanitized against embedded raw HTML in the source) --
/// `pre-redo:crates/git-ents-server/src/markdown.rs`'s own `to_html` did
/// the same: it emitted `pulldown_cmark::html::push_html`'s output
/// unescaped, straight into the page.
#[must_use]
pub(crate) fn to_html(source: &str) -> Markup {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS;
    let mut out = String::new();
    html::push_html(&mut out, Parser::new_ext(source, options));
    PreEscaped(out)
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
}
