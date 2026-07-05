//! Markdown rendering via [`pulldown_cmark`].
//!
//! Markdown gets the same treatment AsciiDoc does: a repository's `README.md`
//! renders as the editorial centerpiece of the overview, and `.md` blobs as
//! formatted documents rather than highlighted source. Output is an embedded
//! fragment (no document frame) styled by the page's own stylesheet.

use pulldown_cmark::{Options, Parser, html};

/// File extensions that name a Markdown document.
const EXTENSIONS: [&str; 4] = ["md", "markdown", "mdown", "mkd"];

/// Whether `name` looks like a Markdown file by its extension.
pub(crate) fn is_markdown(name: &str) -> bool {
    name.rsplit_once('.')
        .is_some_and(|(_, ext)| EXTENSIONS.iter().any(|e| ext.eq_ignore_ascii_case(e)))
}

/// Render Markdown `source` to an embedded HTML fragment, with the tables,
/// footnotes, strikethrough, and task-list extensions people expect from
/// forge-flavored Markdown.
///
/// ## Requirements
///
/// @relation(web.render-registry, web.syntax-highlight)
pub(crate) fn to_html(source: &str) -> String {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS;
    let mut out = String::new();
    html::push_html(&mut out, Parser::new_ext(source, options));
    out
}
