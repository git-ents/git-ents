//! Action-shape-derived entity forms: the same `#[derive(Facet)]` action
//! enums the CLI parses (`ents_forge::issue::IssueAction`,
//! `ents_forge::review::ReviewAction`) drive a web form's controls and its
//! parse — one field list, declared on the action variant, obeyed by both
//! frontends (`lens.parity`) instead of a hand-declared form struct per
//! route. [`action_form`] renders one control per variant field
//! (`Vec` → comma-separated input, `bool` → checkbox, an `ents::compose`
//! field → textarea, anything else → text input), with per-field
//! overrides for the controls a page legitimately customizes (a state
//! picker, a verdict picker); [`parse_action`] reads the posted pairs
//! back into the action variant itself. CSRF stays
//! [`crate::pages::csrf_input`]'s hidden field and the handler's
//! `require_csrf` check; the PRG redirect stays the handler's own.
//!
//! Path-bound values (an `args::positional` id, a review's target) never
//! render as controls — the caller appends them to the posted pairs
//! before parsing. `PathBuf`-shaped fields (`--key`, a local signing-key
//! path no browser can supply) are skipped by both directions.

use std::path::PathBuf;

use facet::{Facet, Field, Type, UserType};
use facet_reflect::Partial;
use maud::{Markup, html};

use crate::error::{Error, Result};
use crate::session::Session;

/// The web-varying parts of one derived form: where it posts, what its
/// submit control says, and the per-field prefills and overrides.
pub struct Spec<'a> {
    /// The form's POST target.
    pub action: &'a str,
    /// The submit button's label.
    pub submit: &'a str,
    /// A cancel link's href, rendered beside the submit button.
    pub cancel: Option<&'a str>,
    /// Prefill values by field name (a `Vec` field prefills comma-joined).
    pub values: &'a [(&'a str, String)],
    /// Custom controls by field name; an empty override omits the field.
    pub overrides: &'a [(&'a str, Markup)],
}

/// Render the form `T`'s variant `variant` declares: one derived control
/// per web-suppliable field in declaration order, `spec.overrides`
/// slotted in place, the session's CSRF hidden field leading.
#[must_use]
pub fn action_form<T: Facet<'static>>(variant: &str, session: &Session, spec: &Spec<'_>) -> Markup {
    let fields = variant_fields::<T>(variant).unwrap_or_default();
    html! {
        form method="post" action=(spec.action) {
            (crate::pages::csrf_input(session))
            @for field in fields {
                @if let Some((_, markup)) = spec.overrides.iter().find(|(name, _)| *name == field.name) {
                    (markup)
                } @else if let Some(markup) = control(field, value_of(spec, field.name)) {
                    (markup)
                }
            }
            @if let Some(cancel) = spec.cancel {
                div.composer-buttons {
                    a.composer-cancel href=(cancel) { "Cancel" }
                    button type="submit" { (spec.submit) }
                }
            } @else {
                button type="submit" { (spec.submit) }
            }
        }
    }
}

/// Parse posted `pairs` (path-bound values appended by the caller) into
/// `T`'s variant `variant`, by the same shape-to-control mapping
/// [`action_form`] renders: a `Vec` splits on commas/whitespace across
/// every posted occurrence, an empty `Option<String>` is `None`, a `bool`
/// is its checkbox's presence, and an unposted field takes its declared
/// default. Unknown pairs (the CSRF token, a `return_to`) are ignored.
///
/// # Errors
///
/// [`Error::InvalidArgument`] if `T` has no such variant, a field's shape
/// is not one this mapping speaks, or the value cannot be set.
pub fn parse_action<T: Facet<'static>>(variant: &str, pairs: &[(String, String)]) -> Result<T> {
    let malformed = |source: &dyn std::fmt::Display| {
        Error::InvalidArgument(format!("malformed {variant} form: {source}"))
    };
    let fields = variant_fields::<T>(variant)?;
    let mut partial = Partial::alloc::<T>()
        .map_err(|source| malformed(&source))?
        .select_variant_named(variant)
        .map_err(|source| malformed(&source))?;
    for (index, field) in fields.iter().enumerate() {
        let posted: Vec<&str> = pairs
            .iter()
            .filter(|(name, _)| name == field.name)
            .map(|(_, value)| value.as_str())
            .collect();
        let shape = field.shape();
        partial = if shape.is_type::<Vec<String>>() {
            partial.set_field(field.name, split_list(&posted))
        } else if let Some(first) = posted.first() {
            if shape.is_type::<String>() {
                partial.set_field(field.name, (*first).to_owned())
            } else if shape.is_type::<Option<String>>() {
                let value = posted.iter().find(|value| !value.trim().is_empty());
                partial.set_field(field.name, value.map(|value| (*value).to_owned()))
            } else if shape.is_type::<bool>() {
                partial.set_field(field.name, matches!(*first, "true" | "on" | "1"))
            } else {
                return Err(Error::InvalidArgument(format!(
                    "unsupported form field: {}",
                    field.name
                )));
            }
        } else {
            partial.set_nth_field_to_default(index)
        }
        .map_err(|source| malformed(&source))?;
    }
    partial
        .build()
        .map_err(|source| malformed(&source))?
        .materialize::<T>()
        .map_err(|source| malformed(&source))
}

/// The posted CSRF token, or empty when the field is absent — feeding
/// `require_csrf`, which then refuses the empty token like any other
/// mismatch.
#[must_use]
pub fn posted_csrf(pairs: &[(String, String)]) -> &str {
    pairs
        .iter()
        .find(|(name, _)| name == crate::session::CSRF_FIELD)
        .map_or("", |(_, value)| value.as_str())
}

/// `variant`'s field list on the action enum `T`.
fn variant_fields<T: Facet<'static>>(variant: &str) -> Result<&'static [Field]> {
    let Type::User(UserType::Enum(shape)) = T::SHAPE.ty else {
        return Err(Error::InvalidArgument(format!(
            "{} is not an action enum",
            T::SHAPE
        )));
    };
    shape
        .variants
        .iter()
        .find(|candidate| candidate.name == variant)
        .map(|found| found.data.fields)
        .ok_or_else(|| Error::InvalidArgument(format!("no such action: {variant}")))
}

/// `field`'s derived control, or `None` for a field the web never
/// renders: an `args::positional` value (bound into the route's own
/// path) or a `PathBuf` (a local file path no browser form supplies).
fn control(field: &Field, value: Option<&str>) -> Option<Markup> {
    if field.has_attr(Some("args"), "positional") {
        return None;
    }
    let shape = field.shape();
    if shape.is_type::<PathBuf>() || shape.is_type::<Option<PathBuf>>() {
        return None;
    }
    let name = field.name;
    let label = title_case(name);
    Some(if shape.is_type::<bool>() {
        html! {
            label {
                input type="checkbox" name=(name) checked[value == Some("true")];
                " " (label)
            }
        }
    } else if shape.is_type::<Vec<String>>() {
        html! {
            label { (label) input type="text" name=(name) value=[value] placeholder="a, b"; }
        }
    } else if field.has_attr(Some("ents"), "compose") {
        html! {
            label { (label) textarea name=(name) { @if let Some(value) = value { (value) } } }
        }
    } else {
        html! {
            label { (label) input type="text" name=(name) value=[value]; }
        }
    })
}

/// `spec.values`'s prefill for `name`, if any.
fn value_of<'a>(spec: &'a Spec<'_>, name: &str) -> Option<&'a str> {
    spec.values
        .iter()
        .find(|(field, _)| *field == name)
        .map(|(_, value)| value.as_str())
}

/// Every posted occurrence split on commas and whitespace, trimmed,
/// empties dropped — one text input carries a whole `Vec` field.
fn split_list(posted: &[&str]) -> Vec<String> {
    posted
        .iter()
        .flat_map(|value| value.split([',', ' ', '\t', '\n']))
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .map(str::to_owned)
        .collect()
}

/// A field name as its control's label: first letter upper-cased.
fn title_case(name: &str) -> String {
    let mut chars = name.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::panic, reason = "unit test")]

    use ents_forge::issue::IssueAction;
    use ents_forge::review::ReviewAction;
    use rstest::rstest;

    use super::*;

    fn session() -> Session {
        Session {
            csrf: "tok".to_owned(),
            member: None,
        }
    }

    fn pairs(entries: &[(&str, &str)]) -> Vec<(String, String)> {
        entries
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    /// The action variant's own shape decides the controls: a compose
    /// field is a textarea, a `Vec` a comma input, a `PathBuf` (`--key`)
    /// and a positional id never render, and an override slots in place.
    #[rstest]
    // @relation(lens.parity, scope=function, role=Verifies)
    fn action_form_derives_controls_from_the_variant_shape() {
        let markup = action_form::<IssueAction>(
            "New",
            &session(),
            &Spec {
                action: "/issues",
                submit: "Open Issue",
                cancel: None,
                values: &[],
                overrides: &[("state", html! { span.custom-state {} })],
            },
        )
        .into_string();
        assert!(markup.contains("<textarea name=\"body\">"));
        assert!(markup.contains("name=\"label\"") && markup.contains("name=\"assignee\""));
        assert!(markup.contains("custom-state") && !markup.contains("name=\"state\""));
        assert!(!markup.contains("name=\"key\""), "{markup}");
        assert!(markup.contains("name=\"csrf\" value=\"tok\""));

        let edit = action_form::<IssueAction>(
            "Edit",
            &session(),
            &Spec {
                action: "",
                submit: "Save",
                cancel: None,
                values: &[("label", "bug, gate".to_owned())],
                overrides: &[],
            },
        )
        .into_string();
        assert!(!edit.contains("name=\"id\""), "positional ids are path-bound");
        assert!(edit.contains("value=\"bug, gate\""));
    }

    /// The same shape parses the post back: `Vec` fields split on commas
    /// and whitespace, an empty optional is `None`, the unposted `--key`
    /// defaults, and unknown pairs (csrf) are ignored.
    #[rstest]
    // @relation(lens.parity, scope=function, role=Verifies)
    fn parse_action_reads_the_posted_pairs_into_the_variant() {
        let action: IssueAction = parse_action(
            "New",
            &pairs(&[
                ("title", "gate rejects a valid signature"),
                ("body", ""),
                ("state", "open"),
                ("label", "bug, gate"),
                ("assignee", "jdc alice"),
                ("csrf", "tok"),
            ]),
        )
        .expect("parses");
        let IssueAction::New {
            title,
            body,
            state,
            label,
            assignee,
            key,
        } = action
        else {
            panic!("wrong variant");
        };
        assert_eq!(title.as_deref(), Some("gate rejects a valid signature"));
        assert_eq!(body, None);
        assert_eq!(state, "open");
        assert_eq!(label, vec!["bug".to_owned(), "gate".to_owned()]);
        assert_eq!(assignee, vec!["jdc".to_owned(), "alice".to_owned()]);
        assert_eq!(key, None);
    }

    /// An unposted field takes the declaration's own default — the same
    /// `default = "open"` the CLI applies to an omitted flag.
    #[rstest]
    // @relation(lens.parity, scope=function, role=Verifies)
    fn parse_action_defaults_an_unposted_field_from_the_declaration() {
        let action: ReviewAction =
            parse_action("New", &pairs(&[("verdict", "approve")])).expect("parses");
        let ReviewAction::New {
            target,
            verdict,
            body,
            key,
        } = action
        else {
            panic!("wrong variant");
        };
        assert_eq!(target, "HEAD");
        assert_eq!(verdict, "approve");
        assert_eq!(body, None);
        assert_eq!(key, None);
    }

    #[rstest]
    fn parse_action_refuses_an_unknown_variant() {
        assert!(parse_action::<IssueAction>("Explode", &[]).is_err());
    }
}
