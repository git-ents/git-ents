//! The editor-file compose flow (`lens.compose`): building the template
//! git-style commit-message file the editor opens, and parsing it back
//! once the user saves.
//!
//! This module is pure — string in, string out, no IO and no git — so the
//! exact template grammar and the "an empty body aborts, `#` lines are
//! ignored" rule are unit-testable on their own, and [`crate::Lens`] owns
//! only the filesystem and mutation halves.
//!
//! # The mechanism, precisely
//!
//! `lens.compose` requires composing to work through a file "the way git
//! itself takes a commit message", using no client-specific extension. The
//! flow the lens drives, using only standard LSP a plain client provides
//! (`workspace/executeCommand`, `window/showDocument`, and
//! `textDocument/didSave`):
//!
//! 1. A `textDocument/codeAction` on the selection returns the
//!    `ents.compose` command.
//! 2. The client runs it via `workspace/executeCommand`; the lens writes
//!    [`template_text`] to `.git/ENTS_COMMENT_EDITMSG` and asks the client
//!    to open it with `window/showDocument`.
//! 3. The user edits the body and saves. The lens's `textDocument/didSave`
//!    handler reads the file, [`parse`]s it, and — if the body is
//!    non-empty — creates the comment through `ents_forge::comment::add`
//!    (the same call the CLI makes, `lens.parity`), anchoring to the
//!    working tree (`lens.working-tree`). An empty body aborts.
//!
//! The template is self-describing: the anchor target (path, lines,
//! working-tree flag, and an optional reply parent) rides in `#`-prefixed
//! metadata lines, so [`parse`] recovers it from the saved file alone and
//! the lens keeps no per-compose state of its own ("owning no state of its
//! own", the lens's whole premise). Because those lines start with `#`
//! they are ignored for the body exactly as any other comment line is, so
//! the metadata can never leak into the comment text.

/// The prefix every machine-readable metadata line in the template carries,
/// after the `#` comment marker: `# ents-compose-<key>: <value>`.
const META_PREFIX: &str = "# ents-compose-";

/// What a compose targets: an anchor (a path, optional `<start>:<end>`
/// lines) captured against the working tree, and/or a reply parent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Target {
    /// Repository-relative path to anchor to, or `None` for a reply that
    /// inherits its aboutness from its parent.
    pub path: Option<String>,
    /// Lines to anchor, as `<start>[:<end>]`.
    pub lines: Option<String>,
    /// Id of the comment being replied to, when this compose is a reply.
    pub parent: Option<String>,
}

/// A parsed, saved template: the body (with `#` lines and surrounding
/// blank lines stripped) and the anchor [`Target`] its metadata named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Composed {
    /// The comment body the user typed. Empty (after trimming) means the
    /// compose was aborted (`lens.compose`).
    pub body: String,
    /// Where the comment anchors / who it replies to.
    pub target: Target,
}

impl Composed {
    /// Whether the saved template aborts the compose: an empty body once
    /// `#` lines and surrounding whitespace are stripped (`lens.compose`).
    #[must_use]
    pub fn is_abort(&self) -> bool {
        self.body.trim().is_empty()
    }
}

/// The initial template text for a new anchored comment on `path`/`lines`
/// against the working tree, or a reply when `target.parent` is set — a
/// blank body followed by git-style `#` guidance and the machine-readable
/// metadata [`parse`] reads back.
#[must_use]
pub fn template_text(target: &Target) -> String {
    let mut out = String::new();
    // One blank line for the body; the user types above the guidance.
    out.push('\n');
    out.push_str("# Leave an ents comment. Lines starting with '#' are ignored;\n");
    out.push_str("# an empty message aborts. Save this file to create the comment.\n");
    out.push_str("#\n");
    match (&target.parent, &target.path) {
        (Some(parent), _) => {
            out.push_str(&format!("# Replying to comment {parent}.\n"));
        }
        (None, Some(path)) => match &target.lines {
            Some(lines) => out.push_str(&format!("# On: {path} lines {lines} (working tree).\n")),
            None => out.push_str(&format!("# On: {path} (working tree).\n")),
        },
        (None, None) => {}
    }
    // Machine-readable metadata: one value per line, so a path containing
    // spaces round-trips without any escaping.
    if let Some(path) = &target.path {
        out.push_str(&format!("{META_PREFIX}path: {path}\n"));
    }
    if let Some(lines) = &target.lines {
        out.push_str(&format!("{META_PREFIX}lines: {lines}\n"));
    }
    if let Some(parent) = &target.parent {
        out.push_str(&format!("{META_PREFIX}parent: {parent}\n"));
    }
    out
}

/// Parse a saved template back into its body and [`Target`]
/// (`lens.compose`): every line starting with `#` is dropped from the body,
/// and the `# ents-compose-<key>: <value>` metadata lines reconstruct the
/// anchor target the compose was started with.
#[must_use]
pub fn parse(content: &str) -> Composed {
    let mut target = Target::default();
    let mut body_lines: Vec<&str> = Vec::new();
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix(META_PREFIX) {
            if let Some((key, value)) = rest.split_once(':') {
                let value = value.trim().to_owned();
                match key {
                    "path" => target.path = Some(value),
                    "lines" => target.lines = Some(value),
                    "parent" => target.parent = Some(value),
                    _ => {}
                }
            }
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        body_lines.push(line);
    }
    let body = body_lines.join("\n").trim().to_owned();
    Composed { body, target }
}
