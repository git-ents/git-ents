//! Turning typed `refs/meta/*` documents into HTML.
//!
//! Every value the meta refs carry is a [`facet::Facet`] type, so one structural
//! walk over its reflected shape can render any of them: a struct becomes a row
//! (its first field the key, the rest a muted value), a map becomes one keyed
//! row per entry, a list stacks its items, and a scalar is its text. That walk
//! is the [`Render`] trait's default, so a new meta-ref type renders for free.
//! Types whose presentation needs domain knowledge — a signer's shortened key, a
//! run's one-line summary — override [`Render::render`] instead.

use std::path::Path;

use facet::{Def, Facet, Peek, Type, UserType};
use maud::{Markup, PreEscaped, html};

use git_ents::checks::{Check, Run, RunOutcome, Status};
use git_ents::config::{Config, RoleRules};
use git_ents::issues::Issue;
use git_ents::members::Member;

use super::component::WebComponent;
use crate::asciidoc;

/// HTML rendering for a meta-ref value. The default walks the value's [`Facet`]
/// shape structurally; a type overrides [`render`](Render::render) when its
/// presentation needs more than the structure carries.
pub(super) trait Render: for<'a> Facet<'a> {
    /// Render `self` as a fragment of card rows.
    fn render(&self) -> Markup {
        render_peek(Peek::new(self))
    }
}

/// A check's name is the key and its command the value — `(composite)` for a
/// check with none — with its image and dependencies appended as ` · `-joined
/// annotations rather than the raw `Option`/`Vec` the structural walk would
/// print.
impl Render for Check {
    fn render(&self) -> Markup {
        let mut value = self
            .command
            .clone()
            .unwrap_or_else(|| "(composite)".to_owned());
        if let Some(image) = &self.image {
            value.push_str(&format!(" · image {image}"));
        }
        if !self.depends.is_empty() {
            value.push_str(&format!(" · needs {}", self.depends.join(", ")));
        }
        row(&self.name, &value)
    }
}

impl WebComponent for Check {
    const TITLE: &'static str = "Checks";

    fn empty() -> Markup {
        html! { div.card-row.muted { "No checks configured on " code { "refs/meta/checks" } "." } }
    }

    fn load(repo: &Path) -> Result<Vec<Self>, String> {
        git_ents::checks::load(repo).map_err(|err| err.to_string())
    }
}

/// Config's editable fields (description, homepage, topics) get their own
/// edit-form treatment in the settings page, so the only piece left to render
/// here is `roles` — one row per role rather than the raw map the structural
/// default would otherwise print.
impl Render for Config {
    fn render(&self) -> Markup {
        html! {
            @for (role, rules) in &self.roles {
                (role_row(role, rules))
            }
        }
    }
}

/// One role's ref-push gating: its `allow`/`deny` glob lists joined for
/// display, or "no rules" for a role entry with neither (matches every ref,
/// same as no entry at all).
fn role_row(role: &str, rules: &RoleRules) -> Markup {
    let mut parts = Vec::new();
    if !rules.allow.is_empty() {
        parts.push(format!("allow {}", rules.allow.join(", ")));
    }
    if !rules.deny.is_empty() {
        parts.push(format!("deny {}", rules.deny.join(", ")));
    }
    if parts.is_empty() {
        parts.push("no rules".to_owned());
    }
    row(role, &parts.join(" · "))
}

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

impl WebComponent for Issue {
    const TITLE: &'static str = "Bug reports";

    fn empty() -> Markup {
        html! { div.card-row.muted { "No bug reports yet." } }
    }

    fn load(repo: &Path) -> Result<Vec<Self>, String> {
        Ok(git_ents::issues::list(repo)
            .map_err(|err| err.to_string())?
            .into_iter()
            .map(|(_id, issue)| issue)
            .collect())
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

impl WebComponent for Member {
    const TITLE: &'static str = "Members";

    fn empty() -> Markup {
        html! {
            div.card-row.muted { "No members — pushes are open until the first key is added." }
        }
    }

    fn load(repo: &Path) -> Result<Vec<Self>, String> {
        git_ents::members::load_all(repo).map_err(|err| err.to_string())
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

/// Whether `status` is still on its way to a terminal outcome — the check has
/// no recording yet, but its run page has a live view worth linking to.
pub(super) fn is_in_progress(status: Status) -> bool {
    matches!(status, Status::Queued | Status::Running)
}

/// A colored status word, shared by the checks-list row and the full
/// recording page so the two agree on how a status reads: green for a pass,
/// red for a failure, muted for anything still settling or skipped.
fn status_badge(status: Status) -> Markup {
    let class = match status {
        Status::Pass => "status-pass",
        Status::Fail | Status::Error => "status-fail",
        Status::Queued | Status::Running | Status::Skipped => "status-pending",
    };
    html! { span class=(class) { (status.to_string()) } }
}

/// One check's row on the "Checks on HEAD" card: a status badge, linked to
/// `href` when there's a live view or a recording behind it, or "no run yet"
/// when `outcome` is absent (just added, or its run has not landed).
pub(super) fn check_list_row(outcome: Option<&RunOutcome>, href: &str) -> Markup {
    html! {
        @match outcome {
            None => span.muted { "no run yet" }
            Some(outcome) if outcome.recording.is_some() || is_in_progress(outcome.status) => {
                a href=(href) { (status_badge(outcome.status)) }
            }
            Some(outcome) => (status_badge(outcome.status))
        }
    }
}

/// The full recording-page body for a settled check: a summary line (status,
/// exit code, duration) plus the terminal — a replay player, or a no-output
/// notice when there is nothing worth replaying — and, when there's a
/// recording, a link to download the raw asciicast.
pub(super) fn check_result_view(outcome: &RunOutcome, download_href: &str) -> Markup {
    html! {
        div.check-summary {
            (status_badge(outcome.status))
            @if let Some(code) = outcome.exit_code {
                span.muted { "exit code " code { (code) } }
            }
            @if let Some(secs) = outcome.duration_secs {
                span.muted { (secs) "s" }
            }
            @if outcome.recording.is_some() {
                a.btn-quiet href=(download_href) download { "Download asciicast" }
            }
        }
        (settled_terminal(outcome))
    }
}

/// The terminal for a settled check: the replay player when the recording has
/// visible output, or the exit-code notice when it doesn't (including when
/// there's no recording at all, or acdc fails to render one) — acdc's replay
/// player renders a bare empty box with no explanation otherwise.
fn settled_terminal(outcome: &RunOutcome) -> Markup {
    let Some(recording) = &outcome.recording else {
        return no_output_notice(outcome);
    };
    if asciidoc::recording_has_no_output(recording) {
        return no_output_notice(outcome);
    }
    match asciidoc::render_recording(recording) {
        Some(player) => html! {
            style { (PreEscaped(asciidoc::TERMINAL_VIEW_CSS)) }
            (PreEscaped(player))
        },
        None => no_output_notice(outcome),
    }
}

/// The best-possible-UX fallback for a settled check with nothing to replay:
/// its exit code when the command actually ran, or just its status when it
/// didn't (a composite, or an infra failure before any command started).
fn no_output_notice(outcome: &RunOutcome) -> Markup {
    html! {
        @match outcome.exit_code {
            Some(code) => p.muted { "Check finished with exit code " code { (code) } " without output." }
            None => p.muted { "This check produced no terminal output." }
        }
    }
}

/// The live-terminal container's inner markup for one poll: the check's
/// current screen, rendered as a static snapshot (see
/// [`asciidoc::render_live`]), or a placeholder while output has yet to
/// arrive.
pub(super) fn live_fragment_body(recording: Option<String>) -> Markup {
    let rendered = recording
        .filter(|recording| !asciidoc::recording_has_no_output(recording))
        .and_then(|recording| asciidoc::render_live(&recording));
    match rendered {
        Some(player) => html! { (PreEscaped(player)) },
        None => html! { p.muted { "Waiting for output…" } },
    }
}
