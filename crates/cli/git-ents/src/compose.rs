//! Attribute-driven `$GIT_EDITOR`/`$EDITOR` composition: an action
//! variant marks its message-carrying fields `#[facet(ents::compose)]`,
//! and this module — reading only the variant's [`facet::Shape`], never a
//! per-command branch — opens the editor when those flags were omitted,
//! mirroring `git commit`'s own editor fallback and its empty-message
//! abort. A frontend concern, deliberately not an `ents-forge` operation
//! (`lens.parity`).

use std::io::Write as _;
use std::process::Command;

use facet::{Facet, Type, UserType};

use crate::error::{Error, Result};

/// Resolve `title` and `body` for a variant whose `title` and `body`
/// fields are compose-marked: given values pass through; with no title,
/// the editor composes both (first line title, rest body).
///
/// # Errors
///
/// [`Error::InvalidArgument`] if the variant's fields are not
/// compose-marked (the flag is simply required) or the composed title is
/// empty; [`Error::Io`] if the editor cannot run.
pub fn title_body<T: Facet<'static>>(
    variant: &str,
    title: Option<String>,
    body: Option<String>,
) -> Result<(String, String)> {
    if let Some(title) = title {
        return Ok((title, body.unwrap_or_default()));
    }
    require_compose::<T>(variant, "title")?;
    let message = editor_message(
        "# First line is the title, the rest is the body.\n\
         # Lines starting with '#' are stripped; an empty title aborts.",
    )?;
    let mut lines = message.lines();
    let title = lines.next().unwrap_or("").trim();
    if title.is_empty() {
        return Err(Error::InvalidArgument("empty message, aborting".into()));
    }
    let body = lines.collect::<Vec<_>>().join("\n");
    Ok((title.to_owned(), body.trim().to_owned()))
}

/// Resolve `body` for a variant whose `body` field is compose-marked:
/// a given value passes through; with none, the editor composes it.
///
/// # Errors
///
/// [`Error::InvalidArgument`] if the field is not compose-marked (the
/// flag is simply required) or the composed body is empty; [`Error::Io`]
/// if the editor cannot run.
pub fn body<T: Facet<'static>>(variant: &str, body: Option<String>) -> Result<String> {
    if let Some(body) = body {
        return Ok(body);
    }
    require_compose::<T>(variant, "body")?;
    let message = editor_message(
        "# Compose the body. Lines starting with '#' are stripped;\n\
         # an empty body aborts.",
    )?;
    let body = message.trim();
    if body.is_empty() {
        return Err(Error::InvalidArgument("empty message, aborting".into()));
    }
    Ok(body.to_owned())
}

/// Refuse unless `T`'s variant marks `field` with `ents::compose` — the
/// attribute on the action enum, not this module, is what licenses the
/// editor fallback; an unmarked omitted flag is simply a missing argument.
fn require_compose<T: Facet<'static>>(variant: &str, field: &str) -> Result<()> {
    let marked = match T::SHAPE.ty {
        Type::User(UserType::Enum(shape)) => shape
            .variants
            .iter()
            .find(|candidate| candidate.name == variant)
            .is_some_and(|found| {
                found
                    .data
                    .fields
                    .iter()
                    .any(|f| f.name == field && f.has_attr(Some("ents"), "compose"))
            }),
        _ => false,
    };
    if marked {
        Ok(())
    } else {
        Err(Error::InvalidArgument(format!("--{field} is required")))
    }
}

/// Open `$GIT_EDITOR` (or `$EDITOR`, or `vi`) on a scratch file seeded
/// with `instructions`, returning its content with `#` lines stripped.
fn editor_message(instructions: &str) -> Result<String> {
    let editor = std::env::var("GIT_EDITOR")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_owned());

    let io_error = |path: &std::path::Path| {
        let path = path.to_owned();
        move |source| Error::Io { path, source }
    };
    let mut file = tempfile::NamedTempFile::new().map_err(io_error(&std::env::temp_dir()))?;
    let path = file.path().to_owned();
    writeln!(file, "\n{instructions}").map_err(io_error(&path))?;
    file.flush().map_err(io_error(&path))?;

    let status = Command::new(&editor)
        .arg(&path)
        .status()
        .map_err(io_error(&path))?;
    if !status.success() {
        return Err(Error::Io {
            path,
            source: std::io::Error::other(format!("{editor} exited with {status}")),
        });
    }

    let contents = std::fs::read_to_string(&path).map_err(io_error(&path))?;
    Ok(contents
        .lines()
        .filter(|line| !line.starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n"))
}
