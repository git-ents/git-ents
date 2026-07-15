//! Turning projected comments and threads into the LSP values the lens
//! publishes: code lenses (`lens.lenses`), hint diagnostics
//! (`lens.diagnostics`), and hover markup (`lens.hover`).
//!
//! Pure rendering only — every input is already-derived data (a `Listed`
//! row, a thread), so the mapping from a comment to its on-screen shape is
//! unit-testable without a repository, and [`crate::Lens`] owns the
//! per-request derivation that feeds it.

use ents_anchor::{Anchor, LineRange, Projection};
use ents_forge::comment::Comment;
use lsp_types::{
    CodeLens, Command, Diagnostic, DiagnosticSeverity, MarkupContent, MarkupKind, Position, Range,
};
use serde_json::json;

/// The `workspace/executeCommand` command that opens the thread
/// (`lens.lenses`: the view operation).
pub const CMD_VIEW: &str = "ents.view";
/// The command that starts a reply compose (`lens.lenses`, `lens.compose`).
pub const CMD_REPLY: &str = "ents.reply";
/// The command that resolves a comment (`lens.lenses`,
/// `model.comment-state`).
pub const CMD_RESOLVE: &str = "ents.resolve";
/// The command that opens the compose template for a new comment
/// (`lens.compose`).
pub const CMD_COMPOSE: &str = "ents.compose";

/// The diagnostic/lens source label the lens stamps every item with, so a
/// client can suppress just the conversation (`lens.diagnostics`).
pub const SOURCE: &str = "ents";

/// Where a projected comment lands on the open document, and whether its
/// anchored lines were edited out from under it (`Projection::Outdated`) —
/// `None` when the comment does not project onto the document at all
/// (`Projection::Deleted`, or no anchor), so the caller omits it
/// (`lens.lenses`).
#[must_use]
pub fn landed_range(projection: &Projection, anchor: &Anchor) -> Option<(Range, bool)> {
    match projection {
        Projection::Current => Some((line_range(anchor.lines), false)),
        Projection::Relocated { lines, .. } => Some((line_range(*lines), false)),
        Projection::Outdated { .. } => Some((line_range(anchor.lines), true)),
        Projection::Deleted => None,
    }
}

/// The half-open LSP [`Range`] covering a 1-based inclusive line range, or
/// the document's first line for a whole-file anchor (`lines` is `None`).
fn line_range(lines: Option<LineRange>) -> Range {
    let (start, end) = match lines {
        Some(range) => (
            to_u32(range.start.saturating_sub(1)),
            to_u32(range.end.saturating_sub(1)),
        ),
        None => (0, 0),
    };
    Range {
        start: Position {
            line: start,
            character: 0,
        },
        // Extend to the end of the last line so a diagnostic underlines the
        // whole anchored region; clients clamp the character to line length.
        end: Position {
            line: end,
            character: u32::MAX,
        },
    }
}

fn to_u32(value: u64) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// A one-line summary of a comment body for a lens title or diagnostic
/// message: the first non-empty line, trimmed and capped so it fits inline.
#[must_use]
pub fn summary(body: &str) -> String {
    const CAP: usize = 60;
    let first = body
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim();
    let mut chars = first.chars();
    let capped: String = chars.by_ref().take(CAP).collect();
    if chars.next().is_some() {
        format!("{capped}…")
    } else {
        capped
    }
}

/// The code lenses for one open root comment at `range` (`lens.lenses`):
/// a primary lens identifying the comment and summarizing its body, then a
/// Reply and a Resolve lens — the thread's operations offered as commands
/// that call the same library operations the CLI exposes (`lens.parity`).
#[must_use]
pub fn code_lenses(id: &str, comment: &Comment, range: Range, outdated: bool) -> Vec<CodeLens> {
    let mut title = format!("💬 {}: {}", short(id), summary(&comment.body));
    if outdated {
        title.push_str(" (outdated)");
    }
    let arg = vec![json!(id)];
    vec![
        CodeLens {
            range,
            command: Some(Command {
                title,
                command: CMD_VIEW.to_owned(),
                arguments: Some(arg.clone()),
            }),
            data: None,
        },
        CodeLens {
            range,
            command: Some(Command {
                title: "Reply".to_owned(),
                command: CMD_REPLY.to_owned(),
                arguments: Some(arg.clone()),
            }),
            data: None,
        },
        CodeLens {
            range,
            command: Some(Command {
                title: "Resolve".to_owned(),
                command: CMD_RESOLVE.to_owned(),
                arguments: Some(arg),
            }),
            data: None,
        },
    ]
}

/// The hint-severity diagnostic mirroring one open comment at `range`
/// (`lens.diagnostics`): the same conversation the code lens carries, for
/// clients that do not render lenses. Never a warning or an error.
#[must_use]
pub fn diagnostic(id: &str, comment: &Comment, range: Range, outdated: bool) -> Diagnostic {
    let mut message = format!("{}: {}", short(id), summary(&comment.body));
    if outdated {
        message.push_str(" (outdated — the anchored lines changed)");
    }
    Diagnostic {
        range,
        // `lens.diagnostics` is binding: conversation carries no judgment,
        // so this is always a hint, never a warning or error.
        severity: Some(DiagnosticSeverity::HINT),
        code: None,
        code_description: None,
        source: Some(SOURCE.to_owned()),
        message,
        related_information: None,
        tags: None,
        data: None,
    }
}

/// The hover markup for a thread (`lens.hover`): every comment in it —
/// bodies, states, and authorship — rendered as Markdown so the whole
/// conversation is readable in the buffer. `rows` are `(id, comment,
/// author, when)` in thread order; `author`/`when` come from each ref's
/// mutation commit chain (`meta-ref.identity-binding`), read by the caller.
#[must_use]
pub fn hover_markup(rows: &[(String, Comment, String, String)]) -> MarkupContent {
    let mut value = String::new();
    for (index, (id, comment, author, when)) in rows.iter().enumerate() {
        if index > 0 {
            value.push_str("\n---\n\n");
        }
        let reply = if comment.parent.is_some() { "↳ " } else { "" };
        value.push_str(&format!(
            "**{reply}{author}** · `{}` · _{}_ · {when}\n\n",
            comment.state,
            short(id)
        ));
        value.push_str(comment.body.trim());
        value.push('\n');
    }
    MarkupContent {
        kind: MarkupKind::Markdown,
        value,
    }
}

/// A comment id shortened for display — the first seven characters, the
/// same length git uses for a short object id.
fn short(id: &str) -> &str {
    id.get(..7).unwrap_or(id)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "unit test")]

    use super::*;

    fn comment(body: &str, state: &str) -> Comment {
        Comment {
            body: body.to_owned(),
            state: state.to_owned(),
            anchor: None,
            context: None,
            parent: None,
        }
    }

    #[test]
    // @relation(lens.diagnostics, scope=function, role=Verifies)
    fn a_diagnostic_is_always_a_hint() {
        let diag = diagnostic(
            "abc1234def",
            &comment("hi", "open"),
            line_range(None),
            false,
        );
        assert_eq!(diag.severity, Some(DiagnosticSeverity::HINT));
        assert_eq!(diag.source.as_deref(), Some(SOURCE));
    }

    #[test]
    // @relation(lens.lenses, scope=function, role=Verifies)
    fn lenses_offer_view_reply_resolve() {
        let lenses = code_lenses(
            "abc1234def",
            &comment("body text", "open"),
            line_range(None),
            false,
        );
        let commands: Vec<&str> = lenses
            .iter()
            .filter_map(|lens| lens.command.as_ref().map(|c| c.command.as_str()))
            .collect();
        assert_eq!(commands, vec![CMD_VIEW, CMD_REPLY, CMD_RESOLVE]);
        let primary = lenses.first().unwrap().command.as_ref().unwrap();
        assert!(primary.title.contains("body text"));
    }

    #[test]
    fn summary_caps_and_takes_the_first_nonempty_line() {
        assert_eq!(summary("\n\nfirst real line\nsecond"), "first real line");
        let long = "x".repeat(80);
        assert!(summary(&long).ends_with('…'));
    }

    #[test]
    // @relation(anchor.projection, scope=function, role=Verifies)
    fn deleted_projection_does_not_land() {
        let anchor_lines = None;
        let range = line_range(anchor_lines);
        // Current lands; Deleted does not.
        assert!(landed_range(&Projection::Current, &fake_anchor()).is_some());
        assert!(landed_range(&Projection::Deleted, &fake_anchor()).is_none());
        let _ = range;
    }

    fn fake_anchor() -> Anchor {
        // A whole-file anchor is enough for `landed_range`, which only reads
        // `anchor.lines`.
        let dir = tempfile::tempdir().unwrap();
        std::process::Command::new("git")
            .arg("init")
            .arg("-q")
            .arg(dir.path())
            .status()
            .unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\n").unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["add", "-A"])
            .status()
            .unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@e.com",
                "commit",
                "-q",
                "-m",
                "x",
            ])
            .status()
            .unwrap();
        let repo = gix::open(dir.path()).unwrap();
        ents_anchor::capture(&repo, "HEAD", "f.txt", None).unwrap()
    }
}
