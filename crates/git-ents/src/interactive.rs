//! Prompting for `add` commands left with unset fields.
//!
//! An omitted field is filled interactively when the terminal supports it,
//! so `git ents effect add` alone walks a user through every field; a script
//! or CI invocation without a TTY gets a clear error instead of a hang.

use std::io::IsTerminal as _;

// @relation(cli.interactive)
/// Whether prompting is possible: both stdin and stdout are a terminal.
#[must_use]
pub fn available() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// `existing`, or a required text prompt for `message` when interactive; an
/// error naming `message` when not, so a script never hangs on a missing
/// argument. An empty string — whether passed explicitly or typed at the
/// prompt — is rejected the same as a missing value.
///
/// ## Requirements
///
/// @relation(cli.interactive)
pub fn text_or(existing: Option<String>, message: &str) -> Result<String, String> {
    if let Some(value) = existing {
        return if value.is_empty() {
            Err(format!("{message} must not be empty"))
        } else {
            Ok(value)
        };
    }
    if !available() {
        return Err(format!(
            "{message} is required (not an interactive terminal)"
        ));
    }
    let value = inquire::Text::new(message)
        .prompt()
        .map_err(|error| error.to_string())?;
    if value.is_empty() {
        return Err(format!("{message} must not be empty"));
    }
    Ok(value)
}

/// `existing`, or an optional text prompt for `message` when interactive —
/// an empty reply is `None`. Non-interactive with no `existing` value stays
/// `None` rather than erroring, since the field is optional.
///
/// ## Requirements
///
/// @relation(cli.interactive)
pub fn optional_text_or(existing: Option<String>, message: &str) -> Result<Option<String>, String> {
    if existing.is_some() {
        return Ok(existing);
    }
    if !available() {
        return Ok(None);
    }
    let value = inquire::Text::new(message)
        .prompt()
        .map_err(|error| error.to_string())?;
    Ok((!value.is_empty()).then_some(value))
}

/// A `Select` prompt among `options`, run only when interactive; `default`
/// otherwise.
///
/// ## Requirements
///
/// @relation(cli.interactive)
pub fn select_or(message: &str, options: &[&str], default: usize) -> Result<usize, String> {
    if !available() {
        return Ok(default);
    }
    let choice = inquire::Select::new(message, options.to_vec())
        .prompt()
        .map_err(|error| error.to_string())?;
    Ok(options
        .iter()
        .position(|option| *option == choice)
        .unwrap_or(default))
}
