//! `GET /files`, `GET /files/{*path}`: a read-only directory listing and
//! blob viewer over the `HEAD` tree of the repository `git ents serve` is
//! serving. A `.md` blob renders via [`crate::markdown`], a
//! `.adoc`/`.asciidoc`/`.asc`/`.adc` blob via [`crate::asciidoc`], and
//! everything else as a line-numbered source view, syntax-highlighted via
//! [`arborium`] when its filename maps to a known grammar (ported from
//! `pre-redo:crates/git-ents-server/src/web/pages.rs`'s own `highlight`;
//! see [`highlight`]'s own doc), escaped plain text otherwise.
//!
//! Tree/blob reads go through `gix`'s high-level `Repository`/`Tree`/`Blob`
//! types (`repo.head_tree()`, `Tree::lookup_entry_by_path`,
//! `Entry::object`), opened fresh per request from `state.path` -- the
//! same `gix::open(repo_path)` pattern `ents_forge::comment::add`/`show`
//! already use to browse a live working tree, not the
//! `facet-git-tree`/`gix_object::Find` convention the rest of this crate's
//! pages use to read typed meta-ref entities (`facet-git-tree` is for
//! structured meta-ref data; browsing arbitrary repository content is not
//! that).
//!
//! A blob view also loads and renders the comments anchored to it
//! (`crate::pages::comments::for_path`), and [`crumbs`] grows a "comment on
//! this file" link (plus a jump to the first card, once there is at least
//! one) beside its own trailing "history" link -- a directory listing
//! carries neither, since a comment anchors to a file, never a tree. A
//! raw-source view (not a rendered document or a binary placeholder)
//! interleaves each comment's card directly after the row naming its
//! anchored range's last line, full width across the blob's line-number
//! and code columns ([`source_view`]); a comment with no current line
//! range (a whole-file anchor, or `ents_anchor::Projection::Outdated`) has
//! nowhere to interleave, and renders in a below-the-blob "outdated
//! comments" section instead ([`outdated_comments_section`]). Doc-rendered
//! and binary views keep every comment below the blob, unconditionally
//! (`crate::pages::comments::comments_section`), since there is no source
//! line to interleave at.

use std::sync::Arc;

use arborium::{Config, Highlighter, HtmlFormat};
use axum::extract::{Path, State};
use gix::bstr::ByteSlice as _;
use gix_object::{Find, Write};
use maud::{Markup, PreEscaped, html};

use crate::assets;
use crate::error::{Error, Result};
use crate::state::AppState;

/// `GET /files`: the repository root directory listing.
///
/// # Errors
///
/// Propagates a `gix::open`/tree-read failure.
pub async fn root<O>(State(state): State<Arc<AppState<O>>>) -> Result<Markup>
where
    O: Find + Write + Send + 'static,
{
    at(&state, "")
}

/// `GET /files/{*path}`: a directory listing or blob view at `path`.
///
/// # Errors
///
/// [`Error::NotFound`] if `path` does not name a tree or blob entry (or
/// contains a `.`/`..` component); otherwise propagates a
/// `gix::open`/tree-read failure.
pub async fn show<O>(
    State(state): State<Arc<AppState<O>>>,
    Path(path): Path<String>,
) -> Result<Markup>
where
    O: Find + Write + Send + 'static,
{
    at(&state, &path)
}

/// The shared implementation behind [`root`] and [`show`]: resolve `path`
/// against `HEAD`'s tree and render whichever of a directory listing or a
/// blob view it names.
fn at<O>(state: &AppState<O>, path: &str) -> Result<Markup>
where
    O: Find + Write,
{
    if !is_safe_path(path) {
        return Err(Error::NotFound {
            what: path.to_owned(),
        });
    }

    let repo = gix::open(&state.path).map_err(|source| Error::Repo(source.to_string()))?;
    let head_tree = match repo.head_tree() {
        Ok(tree) => tree,
        // An unborn HEAD (a freshly initialized, still-empty repository)
        // reads as an empty root directory, not a failure -- mirrors
        // `pre-redo:crates/git-ents-server/src/web/git.rs`'s `root_tree`,
        // which returned an empty entry list rather than erroring when the
        // repository had no `HEAD` yet.
        Err(_) if path.is_empty() => {
            return Ok(super::layout(
                &super::RepoHeader::from_state(state),
                &super::identity_label(state),
                super::Tab::Files,
                "files",
                html! {
                    (crumbs(path, None))
                    (dir_listing(path, Vec::new()))
                },
            ));
        }
        Err(_) => {
            return Err(Error::NotFound {
                what: path.to_owned(),
            });
        }
    };

    if path.is_empty() {
        let entries = tree_entries(&head_tree)?;
        return Ok(super::layout(
            &super::RepoHeader::from_state(state),
            &super::identity_label(state),
            super::Tab::Files,
            "files",
            html! {
                (crumbs(path, None))
                (dir_listing(path, entries))
            },
        ));
    }

    let entry = head_tree
        .lookup_entry_by_path(path)
        .map_err(|source| Error::Repo(source.to_string()))?
        .ok_or_else(|| Error::NotFound {
            what: path.to_owned(),
        })?;

    if entry.mode().is_tree() {
        let subtree = entry
            .object()
            .map_err(|source| Error::Repo(source.to_string()))?
            .try_into_tree()
            .map_err(|source| Error::Repo(source.to_string()))?;
        let entries = tree_entries(&subtree)?;
        Ok(super::layout(
            &super::RepoHeader::from_state(state),
            &super::identity_label(state),
            super::Tab::Files,
            path,
            html! {
                (crumbs(path, None))
                (dir_listing(path, entries))
            },
        ))
    } else if entry.mode().is_blob() {
        let blob = entry
            .object()
            .map_err(|source| Error::Repo(source.to_string()))?
            .try_into_blob()
            .map_err(|source| Error::Repo(source.to_string()))?;
        let name = path.rsplit('/').next().unwrap_or(path);
        let comments = super::comments::for_path(state, &repo, path);
        let (body, below) = blob_view(name, &blob.data, &comments)?;
        Ok(super::layout(
            &super::RepoHeader::from_state(state),
            &super::identity_label(state),
            super::Tab::Files,
            path,
            html! {
                (crumbs(path, Some(comments.len())))
                (body)
                (below)
            },
        ))
    } else {
        // A symlink or a submodule (gitlink) -- neither is a tree or a
        // blob this browser can render.
        Err(Error::NotFound {
            what: path.to_owned(),
        })
    }
}

/// Whether `path` is safe to resolve against a tree: no empty, `.`, or
/// `..` component. The empty root path is itself safe.
fn is_safe_path(path: &str) -> bool {
    path.is_empty()
        || path
            .split('/')
            .all(|s| !s.is_empty() && s != "." && s != "..")
}

/// One `(name, is_directory)` pair per direct child of `tree`, in tree
/// order (not yet sorted -- [`dir_listing`] sorts for display).
fn tree_entries(tree: &gix::Tree<'_>) -> Result<Vec<(String, bool)>> {
    tree.iter()
        .map(|entry| {
            let entry = entry.map_err(|source| Error::Repo(source.to_string()))?;
            Ok((
                entry.filename().to_str_lossy().into_owned(),
                entry.mode().is_tree(),
            ))
        })
        .collect()
}

/// The link to a child of the directory at `dir` (empty at the root).
fn child_href(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        format!("/files/{name}")
    } else {
        format!("/files/{dir}/{name}")
    }
}

/// A directory listing at `dir`: entries sorted directories-first then
/// alphabetically, each an icon and a link one level deeper.
fn dir_listing(dir: &str, mut entries: Vec<(String, bool)>) -> Markup {
    entries.sort_by(|(a_name, a_is_dir), (b_name, b_is_dir)| {
        b_is_dir.cmp(a_is_dir).then_with(|| a_name.cmp(b_name))
    });
    html! {
        div.card {
            div.card-header { "files" }
            @if entries.is_empty() {
                div.card-row.muted { "Empty directory." }
            }
            @for (name, is_dir) in &entries {
                div.card-row.is-dir[*is_dir] {
                    a.row-link href=(child_href(dir, name)) {
                        @if *is_dir { (assets::icon_folder()) } @else { (assets::icon_file()) }
                        (name)
                    }
                }
            }
        }
    }
}

/// Breadcrumb navigation from the repository's files root down through
/// `path`, `chevron-right` icons separating segments, plus trailing links
/// into `crate::pages::commits`'s `GET /commits` history (the file
/// browser's one entry point into commit history, since history is a view
/// of the code, not a tab of its own -- `crate::pages::mod`'s own doc) and,
/// on a blob view (`comments` is `Some`), `crate::pages::comments`'s own
/// add form for this file ("comment on this file") plus a jump straight to
/// the first comment card (`id="comment-0"`, in display order -- see
/// [`super::comments::comment_card`]'s own doc) when there is at least one
/// comment already anchored here, wherever it renders (inline or below the
/// blob). `comments` is `None` on a directory listing, where neither link
/// makes sense.
fn crumbs(path: &str, comments: Option<usize>) -> Markup {
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let mut acc = String::new();
    let mut trail: Vec<(String, Option<String>)> =
        vec![("files".to_owned(), Some("/files".to_owned()))];
    for (index, part) in parts.iter().enumerate() {
        if !acc.is_empty() {
            acc.push('/');
        }
        acc.push_str(part);
        let is_last = index.saturating_add(1) == parts.len();
        let href = (!is_last).then(|| format!("/files/{acc}"));
        trail.push(((*part).to_owned(), href));
    }
    html! {
        nav.crumbs {
            @for (index, (label, href)) in trail.iter().enumerate() {
                @if index > 0 { span.sep { (assets::icon_chevron()) } }
                @match href {
                    Some(href) => a href=(href) { (label) },
                    None => span.here { (label) },
                }
            }
            a.crumbs-history href="/commits" { "history" }
            @if let Some(count) = comments {
                a.crumbs-history href={ "/comments?file=" (path) } { "comment on this file" }
                @if count > 0 {
                    a.crumbs-history href="#comment-0" {
                        (count) @if count == 1 { " comment" } @else { " comments" }
                    }
                }
            }
        }
    }
}

/// Whether `bytes` looks like binary content (a NUL byte in the leading
/// chunk -- the same heuristic git itself uses, carried over from
/// `pre-redo:crates/git-ents-server/src/web/pages.rs`'s own `is_binary`).
fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8000).any(|b| *b == 0)
}

/// A single blob's contents, plus whatever comments belong below it: a
/// Markdown/AsciiDoc document rendered as such via
/// [`crate::markdown`]/[`crate::asciidoc`] or a binary-content placeholder
/// -- either way every `comment` renders below, unconditionally
/// ([`crate::pages::comments::comments_section`]), since there is no
/// source line to interleave a card at -- or [`source_view`]'s
/// line-per-row rendering, which interleaves a comment with a current line
/// range directly into the blob and returns the rest (no current line
/// range: a whole-file anchor, or `ents_anchor::Projection::Outdated`) as
/// a separate below-the-blob section ([`outdated_comments_section`]).
///
/// # Errors
///
/// Propagates [`crate::asciidoc::to_html`]'s own [`Error::Asciidoc`].
fn blob_view(
    name: &str,
    bytes: &[u8],
    comments: &[super::comments::FileComment],
) -> Result<(Markup, Markup)> {
    if is_binary(bytes) {
        return Ok((
            html! { div.binary { "Binary file (" (bytes.len()) " bytes) not shown." } },
            super::comments::comments_section(comments),
        ));
    }
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Ok((
            html! { div.binary { "Binary file (" (bytes.len()) " bytes) not shown." } },
            super::comments::comments_section(comments),
        ));
    };
    if crate::markdown::is_markdown(name) {
        return Ok((
            html! { div.card { div.doc-body { (crate::markdown::to_html(text)) } } },
            super::comments::comments_section(comments),
        ));
    }
    if crate::asciidoc::is_asciidoc(name) {
        return Ok((
            html! { div.card { div.doc-body { (crate::asciidoc::to_html(text)?) } } },
            super::comments::comments_section(comments),
        ));
    }
    let highlighted = highlight(name, text);
    let below: Vec<(usize, &super::comments::FileComment)> = comments
        .iter()
        .enumerate()
        .filter(|(_, comment)| comment.lines.is_none())
        .collect();
    Ok((
        source_view(text, highlighted, comments),
        outdated_comments_section(&below),
    ))
}

/// The raw-source view: one table row per line (a `<tr>` pairing a
/// `.blob-nums` line-number cell carrying the row's `#L{n}` anchor with a
/// `.blob-code` cell, no wrapper beyond those two cells -- lean enough that
/// thousands of lines stay cheap), highlighted via [`highlight`] when
/// `highlighted` is `Some` and falling back to plain (still per-line,
/// still auto-escaped by `maud`'s own interpolation) text otherwise. Each
/// [`FileComment`](super::comments::FileComment) in `comments` whose
/// [`ents_anchor::LineRange`] is `Some` renders its card
/// ([`super::comments::comment_card`]) immediately after the row naming
/// its range's last line, full width across both columns
/// (`tr.blob-comment-row`, `colspan="2"`) -- multiple comments ending on
/// the same line stack in `comments`' own order (`comment::list`'s ref
/// order). A comment with no current line range is [`blob_view`]'s own
/// concern, not this function's: it never appears here.
fn source_view(
    text: &str,
    highlighted: Option<String>,
    comments: &[super::comments::FileComment],
) -> Markup {
    let physical_lines: Vec<&str> = text.lines().collect();
    let line_count = physical_lines.len().max(1);

    let mut code_lines: Vec<Markup> = match &highlighted {
        Some(html) => split_highlighted_lines(html, line_count)
            .into_iter()
            .map(|fragment| html! { (PreEscaped(fragment)) })
            .collect(),
        None => physical_lines
            .iter()
            .map(|line| html! { (*line) })
            .collect(),
    };
    // Exactly `line_count` rows either way: arborium trims trailing
    // newlines before highlighting (see `split_highlighted_lines`'s own
    // doc), so a file ending in blank lines can highlight to fewer
    // embedded newlines than `text.lines().count()` -- padding (never
    // truncating in practice, since `split_highlighted_lines` never
    // returns fewer than one fragment) keeps every gutter number paired
    // with a code cell, with no per-row fallback indexing needed below.
    code_lines.resize_with(line_count, Markup::default);

    let mut by_end_line: std::collections::BTreeMap<u64, Vec<usize>> =
        std::collections::BTreeMap::new();
    for (index, comment) in comments.iter().enumerate() {
        if let Some(range) = comment.lines {
            by_end_line.entry(range.end).or_default().push(index);
        }
    }

    html! {
        div.blob {
            table {
                tbody {
                    @for (index, code) in code_lines.into_iter().enumerate() {
                        @let n = index.saturating_add(1);
                        tr {
                            td.blob-nums { a id={ "L" (n) } href={ "#L" (n) } { (n) } }
                            @if highlighted.is_some() {
                                td.blob-code { code.code { (code) } }
                            } @else {
                                td.blob-code { code { (code) } }
                            }
                        }
                        @if let Some(indices) = by_end_line.get(&u64::try_from(n).unwrap_or(u64::MAX)) {
                            @for &comment_index in indices {
                                @if let Some(comment) = comments.get(comment_index) {
                                    tr.blob-comment-row {
                                        td colspan="2" {
                                            (super::comments::comment_card(
                                                comment_index,
                                                comment,
                                                super::comments::LinkMode::SameFile,
                                            ))
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The below-the-blob section for comments with no current line range to
/// interleave at (a whole-file anchor, or
/// `ents_anchor::Projection::Outdated`) -- titled to distinguish it from
/// the inline cards [`source_view`] interleaves directly into the blob,
/// since every comment reaching here either predates line-level anchoring
/// or has literally gone stale. Renders nothing at all when `comments` is
/// empty (mirrors [`super::comments::comments_section`]'s identical
/// stance).
fn outdated_comments_section(comments: &[(usize, &super::comments::FileComment)]) -> Markup {
    if comments.is_empty() {
        return html! {};
    }
    html! {
        h2 { "outdated comments" }
        @for &(index, comment) in comments {
            (super::comments::comment_card(index, comment, super::comments::LinkMode::SameFile))
        }
    }
}

/// Split [`highlight`]'s single HTML string into one HTML fragment per
/// source line (`line_count` of them, padding with an empty string past
/// whatever [`tokenize`] actually produced -- arborium trims trailing
/// newlines from its input before highlighting, so a file ending in
/// several blank lines can highlight to fewer embedded newlines than
/// `text.lines().count()`; [`source_view`]'s own row loop indexes
/// defensively for the same reason).
///
/// The hard part: a highlight span **can** cross a newline (a multiline
/// block comment, a triple-quoted string), so it is not enough to split on
/// `\n` -- a span open at a line boundary must be closed before the split
/// and reopened after it, or the two resulting fragments are not
/// independently well-formed HTML. This walks [`tokenize`]'s token stream
/// with an explicit stack of open span classes: a `Text` token's embedded
/// newlines close every open span, end the current line, and reopen them
/// (in the same order) at the start of the next.
fn split_highlighted_lines(html: &str, line_count: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::with_capacity(line_count.max(1));
    let mut current = String::new();
    let mut open: Vec<&str> = Vec::new();

    for token in tokenize(html) {
        match token {
            Token::Open(class) => {
                current.push_str("<span class=\"");
                current.push_str(class);
                current.push_str("\">");
                open.push(class);
            }
            Token::Close => {
                current.push_str("</span>");
                open.pop();
            }
            Token::Text(text) => {
                let mut parts = text.split('\n');
                if let Some(first) = parts.next() {
                    current.push_str(first);
                }
                for rest in parts {
                    for _ in &open {
                        current.push_str("</span>");
                    }
                    lines.push(std::mem::take(&mut current));
                    for class in &open {
                        current.push_str("<span class=\"");
                        current.push_str(class);
                        current.push_str("\">");
                    }
                    current.push_str(rest);
                }
            }
        }
    }
    lines.push(current);
    lines
}

/// One tokenized fragment of arborium's `HtmlFormat::ClassNames` output
/// (`arborium_highlight::render::spans_to_html`'s own doc): an opening
/// `<span class="...">`, its matching `</span>`, or a run of
/// already-escaped text between tags. That renderer never emits any tag
/// but these two, and every text run it emits is already HTML-escaped
/// (`&lt;`, `&amp;`, ...) -- [`split_highlighted_lines`] never re-escapes
/// or splits an entity, since [`Token::Text`] is only ever split on
/// literal `\n` bytes, never re-parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Token<'a> {
    /// `<span class="{0}">`.
    Open(&'a str),
    /// `</span>`.
    Close,
    /// Already-escaped text between tags.
    Text(&'a str),
}

/// Tokenize `html` into a stream of [`Token`]s -- see [`Token`]'s own doc
/// for why a simple `<span class="...">`/`</span>` scan is sufficient
/// (arborium's own HTML renderer emits no other tag, and every text run is
/// already escaped so it never contains a literal `<`). Malformed input
/// (which arborium's own renderer never produces) degrades to treating the
/// unrecognized byte as plain text rather than panicking or looping
/// forever.
fn tokenize(html: &str) -> Vec<Token<'_>> {
    const OPEN_PREFIX: &str = "<span class=\"";
    const CLOSE_TAG: &str = "</span>";

    let mut tokens = Vec::new();
    let mut rest = html;
    while !rest.is_empty() {
        // `.get(..)`/`.get(n..)` rather than direct indexing throughout:
        // every offset here comes from `find`/`strip_prefix`, always a
        // valid char boundary, but this function still never indexes a
        // `str` directly (`clippy::string_slice`) or performs raw
        // arithmetic on an offset (`clippy::arithmetic_side_effects`) --
        // `.get(end..)` then `strip_prefix('"')` finds "just past the
        // quote" without ever computing `end + 1`.
        if let Some(after_prefix) = rest.strip_prefix(OPEN_PREFIX)
            && let Some(end) = after_prefix.find('"')
            && let Some(class) = after_prefix.get(..end)
            && let Some(after_quote) = after_prefix.get(end..).and_then(|s| s.strip_prefix('"'))
            && let Some(after_gt) = after_quote.strip_prefix('>')
        {
            tokens.push(Token::Open(class));
            rest = after_gt;
            continue;
        }
        if let Some(after) = rest.strip_prefix(CLOSE_TAG) {
            tokens.push(Token::Close);
            rest = after;
            continue;
        }
        let next_tag = [rest.find(OPEN_PREFIX), rest.find(CLOSE_TAG)]
            .into_iter()
            .flatten()
            .min();
        match next_tag {
            Some(0) | None => {
                // No recognized tag anywhere ahead (or, defensively, right
                // at the cursor despite the checks above not matching it
                // -- malformed input arborium never actually produces):
                // take the rest as one text run rather than looping.
                tokens.push(Token::Text(rest));
                rest = "";
            }
            Some(idx) => {
                let text = rest.get(..idx).unwrap_or(rest);
                rest = rest.get(idx..).unwrap_or_default();
                tokens.push(Token::Text(text));
            }
        }
    }
    tokens
}

/// Highlighted HTML for `source`, or `None` when `name`'s extension names
/// no grammar [`arborium::detect_language`] recognizes -- [`blob_view`]
/// then falls back to escaped plain text. Ported from
/// `pre-redo:crates/git-ents-server/src/web/pages.rs`'s own `highlight`,
/// its `HtmlFormat::ClassNames` output matched by
/// `crate::assets::OVERRIDES`'s `.code .keyword`-family rules.
///
/// The [`Highlighter`] is built and used entirely within this synchronous
/// call -- its grammar store is not `Send`, so it must never be held
/// across an `.await` (this function itself is never `async`, and neither
/// is any caller between it and the request handler).
fn highlight(name: &str, source: &str) -> Option<String> {
    let language = arborium::detect_language(name)?;
    let config = Config {
        html_format: HtmlFormat::ClassNames,
        ..Default::default()
    };
    Highlighter::with_config(config)
        .highlight(language, source)
        .ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "unit test")]

    use ents_anchor::LineRange;
    use rstest::rstest;

    use super::*;
    use crate::pages::comments::FileComment;

    /// A minimal [`FileComment`] fixture -- the `body`/`author`/`seconds`
    /// values never matter to a rendering-position assertion, only
    /// `lines`.
    fn comment(lines: Option<LineRange>) -> FileComment {
        FileComment {
            author: "commenter".to_owned(),
            seconds: 0,
            path: "src/main.rs".to_owned(),
            lines,
            outdated: false,
            body: html! { p { "worth a look" } },
        }
    }

    #[rstest]
    #[case::empty("", true)]
    #[case::simple("src/main.rs", true)]
    #[case::nested("a/b/c", true)]
    #[case::dot(".", false)]
    #[case::dotdot("..", false)]
    #[case::traversal("a/../b", false)]
    #[case::trailing_slash("a/", false)]
    #[case::double_slash("a//b", false)]
    fn is_safe_path_rejects_dot_components_and_empty_segments(
        #[case] path: &str,
        #[case] expected: bool,
    ) {
        assert_eq!(is_safe_path(path), expected);
    }

    #[test]
    fn dir_listing_sorts_directories_first_then_alphabetically() {
        let entries = vec![
            ("zeta.txt".to_owned(), false),
            ("alpha".to_owned(), true),
            ("beta.txt".to_owned(), false),
            ("gamma".to_owned(), true),
        ];
        let rendered = dir_listing("", entries).into_string();
        let alpha = rendered.find("alpha").expect("alpha listed");
        let gamma = rendered.find("gamma").expect("gamma listed");
        let beta = rendered.find("beta.txt").expect("beta listed");
        let zeta = rendered.find("zeta.txt").expect("zeta listed");
        assert!(alpha < gamma, "directories sort among themselves");
        assert!(gamma < beta, "every directory sorts before every file");
        assert!(beta < zeta, "files sort among themselves");
    }

    #[test]
    fn blob_view_renders_markdown_as_a_heading_not_raw_markup() {
        let (body, _below) = blob_view("readme.md", b"# Title\n", &[]).expect("markdown renders");
        assert!(body.into_string().contains("<h1>Title</h1>"));
    }

    #[test]
    fn blob_view_renders_asciidoc_as_a_heading_not_raw_markup() {
        let (body, _below) =
            blob_view("readme.adoc", b"= Title\n\nBody.\n", &[]).expect("asciidoc renders");
        assert!(body.into_string().contains("<h1>Title</h1>"));
    }

    #[test]
    fn blob_view_escapes_plain_text_into_a_line_numbered_code_block() {
        let (body, _below) =
            blob_view("notes.txt", b"1 < 2 and true", &[]).expect("plain text renders");
        let rendered = body.into_string();
        assert!(rendered.contains("blob-nums"));
        assert!(rendered.contains("<td class=\"blob-code\"><code>"));
        assert!(rendered.contains("1 &lt; 2"));
    }

    #[test]
    fn blob_view_highlights_a_recognized_language_with_syntax_token_classes() {
        let (body, _below) =
            blob_view("main.rs", b"fn main() { let x = 1; }", &[]).expect("rust renders");
        let rendered = body.into_string();
        assert!(rendered.contains("blob-nums"));
        assert!(rendered.contains("class=\"code\""));
        assert!(rendered.contains("class=\"keyword\""));
    }

    #[test]
    fn blob_view_shows_a_placeholder_for_binary_content() {
        let (body, _below) =
            blob_view("data.bin", b"\0\x01\x02binary", &[]).expect("binary placeholder renders");
        assert!(body.into_string().contains("Binary file"));
    }

    #[test]
    fn blob_view_routes_a_doc_comment_below_the_blob_never_inline() {
        let comments = vec![comment(Some(LineRange { start: 1, end: 1 }))];
        let (_body, below) =
            blob_view("readme.md", b"# Title\n", &comments).expect("markdown renders");
        // A doc view has no source line to interleave at: every comment,
        // even one with a current line range, renders in the below
        // section -- `comments_section`'s plain, untitled list, not
        // `outdated_comments_section`'s titled one.
        assert!(below.into_string().contains("worth a look"));
    }

    #[test]
    fn source_view_interleaves_a_comment_directly_after_its_last_line() {
        let comments = vec![comment(Some(LineRange { start: 1, end: 2 }))];
        let rendered = source_view("line 1\nline 2\nline 3\n", None, &comments).into_string();
        let line2 = rendered.find("id=\"L2\"").expect("line 2 renders");
        let card = rendered.find("comment-meta").expect("card renders");
        let line3 = rendered.find("id=\"L3\"").expect("line 3 renders");
        assert!(
            line2 < card && card < line3,
            "the card lands strictly between line 2 and line 3: {rendered}"
        );
    }

    #[test]
    fn source_view_stacks_multiple_comments_ending_on_the_same_line_in_order() {
        let comments = vec![
            {
                let mut c = comment(Some(LineRange { start: 1, end: 1 }));
                c.body = html! { p { "first" } };
                c
            },
            {
                let mut c = comment(Some(LineRange { start: 1, end: 1 }));
                c.body = html! { p { "second" } };
                c
            },
        ];
        let rendered = source_view("line 1\nline 2\n", None, &comments).into_string();
        let first = rendered.find("first").expect("first comment renders");
        let second = rendered.find("second").expect("second comment renders");
        assert!(first < second, "stacked comments keep ref order");
    }

    #[test]
    fn source_view_omits_a_comment_with_no_current_line_range() {
        let comments = vec![comment(None)];
        let rendered = source_view("line 1\nline 2\n", None, &comments).into_string();
        assert!(
            !rendered.contains("worth a look"),
            "a comment with no lines has nowhere to interleave -- blob_view routes it below instead"
        );
    }

    #[test]
    fn child_href_nests_under_the_current_directory() {
        assert_eq!(child_href("", "src"), "/files/src");
        assert_eq!(child_href("src", "main.rs"), "/files/src/main.rs");
    }

    #[test]
    fn tokenize_splits_spans_and_text_without_touching_entities() {
        let tokens = tokenize("<span class=\"keyword\">fn</span> 1 &lt; 2");
        assert_eq!(
            tokens,
            vec![
                Token::Open("keyword"),
                Token::Text("fn"),
                Token::Close,
                Token::Text(" 1 &lt; 2"),
            ]
        );
    }

    #[test]
    fn split_highlighted_lines_reopens_a_span_that_crosses_a_newline() {
        // A three-line block comment as one span, per arborium's own
        // `spans_to_html` shape (see that function's own tests): one
        // `<span>` whose text contains embedded newlines, followed by an
        // unrelated keyword span on the line after.
        let html = "<span class=\"comment\">/*\nfoo\nbar*/</span>\n<span class=\"keyword\">fn</span> main() {}";
        let lines = split_highlighted_lines(html, 4);
        assert_eq!(
            lines,
            vec![
                "<span class=\"comment\">/*</span>".to_owned(),
                "<span class=\"comment\">foo</span>".to_owned(),
                "<span class=\"comment\">bar*/</span>".to_owned(),
                "<span class=\"keyword\">fn</span> main() {}".to_owned(),
            ],
            "each fragment is independently well-formed and still classed"
        );
    }

    #[test]
    fn split_highlighted_lines_never_re_escapes_or_splits_an_entity() {
        let html = "<span class=\"operator\">&lt;</span>\nnext";
        let lines = split_highlighted_lines(html, 2);
        assert_eq!(
            lines,
            vec![
                "<span class=\"operator\">&lt;</span>".to_owned(),
                "next".to_owned(),
            ]
        );
    }

    #[test]
    fn split_highlighted_lines_handles_plain_unhighlighted_text() {
        let lines = split_highlighted_lines("a\nb\nc", 3);
        assert_eq!(lines, vec!["a".to_owned(), "b".to_owned(), "c".to_owned()]);
    }
}
