//! Turning typed `refs/meta/*` documents into HTML.
//!
//! Every value the meta refs carry is a [`facet::Facet`] type, so one structural
//! walk over its reflected shape can render any of them: a struct becomes a row
//! (its first field the key, the rest a muted value), a map becomes one keyed
//! row per entry, a list stacks its items, and a scalar is its text. That walk
//! is the [`Render`] trait's default, so a new meta-ref type renders for free.
//! Types whose presentation needs domain knowledge — a signer's shortened key, a
//! run's one-line summary — override [`Render::render`] instead.

use facet::{Def, Facet, Peek, Type, UserType};
use maud::{Markup, html};

use git_ents::checks::{Check, Run};
use git_ents::config::Config;
use git_ents::issues::Issue;
use git_ents::members::Member;

/// HTML rendering for a meta-ref value. The default walks the value's [`Facet`]
/// shape structurally; a type overrides [`render`](Render::render) when its
/// presentation needs more than the structure carries.
pub(super) trait Render: for<'a> Facet<'a> {
    /// Render `self` as a fragment of card rows.
    fn render(&self) -> Markup {
        render_peek(Peek::new(self))
    }
}

/// A check renders structurally: its name is the key, its command the value.
impl Render for Check {}

/// Config renders structurally: each field becomes a keyed row.
impl Render for Config {}

/// An issue's title leads the row, its labels render as chips beside it rather
/// than the raw ` · `-joined list the structural walk would print.
impl Render for Issue {
    fn render(&self) -> Markup {
        html! {
            div.card-row.issue-row {
                span.issue-title { (self.title) }
                @for label in &self.labels {
                    span.chip { (label) }
                }
            }
        }
    }
}

/// A member renders one row per authorized key — the username as the key column,
/// a short key label beside it — or a single `cert-authority` row for a pinned
/// CA, rather than the raw keys and trust enum the structural walk would print.
impl Render for Member {
    fn render(&self) -> Markup {
        if let Some(ca) = self.ca() {
            return row(
                &self.principal,
                &format!("cert-authority · {}", signer_label(ca)),
            );
        }
        html! {
            @for (_fingerprint, key) in self.keys() {
                (row(&self.principal, &signer_label(key)))
            }
        }
    }
}

/// A run is a set of per-check outcomes; collapse them to one summary line
/// rather than a row per outcome.
impl Render for Run {
    fn render(&self) -> Markup {
        html! { span.muted { (run_summary(self)) } }
    }
}

/// Render any reflected value as card rows by walking its shape.
fn render_peek(peek: Peek<'_, '_>) -> Markup {
    match peek.shape().def {
        Def::Scalar => return html! { (scalar_text(&peek)) },
        Def::Map(_) => {
            let Ok(map) = peek.into_map() else {
                return html! {};
            };
            return html! {
                @for (key, value) in map.iter() {
                    (row(&scalar_text(&key), &scalar_text(&value)))
                }
            };
        }
        Def::List(_) | Def::Array(_) | Def::Slice(_) => {
            let Ok(list) = peek.into_list_like() else {
                return html! {};
            };
            return html! { @for item in list.iter() { (render_peek(item)) } };
        }
        _ => {}
    }
    if let Type::User(UserType::Struct(st)) = peek.shape().ty {
        let Ok(strukt) = peek.into_struct() else {
            return html! {};
        };
        let mut key = String::new();
        let mut rest: Vec<String> = Vec::new();
        for index in 0..st.fields.len() {
            let Ok(field) = strukt.field(index) else {
                continue;
            };
            let text = scalar_text(&field);
            if index == 0 {
                key = text;
            } else {
                rest.push(text);
            }
        }
        return row(&key, &rest.join(" · "));
    }
    html! {}
}

/// One card row: a key in `code.key`, its value muted beside it.
fn row(key: &str, value: &str) -> Markup {
    html! {
        div.card-row.signer-row {
            code.key { (key) }
            span.muted { (value) }
        }
    }
}

/// The textual form of a scalar peek: its string value, or its `Display`.
fn scalar_text(peek: &Peek<'_, '_>) -> String {
    peek.as_str()
        .map_or_else(|| format!("{peek}"), str::to_owned)
}

/// A short label for a signer's key: its type and trailing comment, dropping the
/// long base64 body that would not fit on the row.
fn signer_label(key: &str) -> String {
    let mut parts = key.split_whitespace();
    let kind = parts.next().unwrap_or_default();
    let comment = parts.nth(1).unwrap_or_default();
    if comment.is_empty() {
        kind.to_owned()
    } else {
        format!("{kind} · {comment}")
    }
}

/// A one-line summary of a run's outcomes, e.g. `fmt pass · test fail`.
fn run_summary(run: &Run) -> String {
    run.results
        .iter()
        .map(|result| format!("{} {}", result.name, result.status))
        .collect::<Vec<_>>()
        .join(" · ")
}
