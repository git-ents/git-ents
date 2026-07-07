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

use git_effect::{Effect, Run, RunOutcome, Status};
use git_ents_core::config::{Config, RoleRules};
use git_ents_core::issues::Issue;
use git_member::members::Member;

use super::component::{Loadable, WebComponent};
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

/// An effect's name is the key and its command the value — `(composite)` for
/// an effect with none — with its image, dependencies, and toolchains
/// appended as ` · `-joined annotations rather than the raw `Option`/`Vec`
/// the structural walk would print.
impl Render for Effect {
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
        if !self.toolchains.is_empty() {
            value.push_str(&format!(" · toolchains {}", self.toolchains.join(", ")));
        }
        row(&self.name, &value)
    }
}

impl Loadable for Effect {
    fn load(repo: &Path) -> Result<Vec<Self>, String> {
        git_effect::load_all(repo).map_err(|err| err.to_string())
    }
}

impl WebComponent for Effect {
    const TITLE: &'static str = "Checks";

    fn empty() -> Markup {
        html! { div.card-row.muted { "No effects configured on " code { "refs/meta/effects" } "." } }
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

impl Loadable for Issue {
    fn load(repo: &Path) -> Result<Vec<Self>, String> {
        Ok(git_ents_core::issues::list(repo)
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

impl Loadable for Member {
    fn load(repo: &Path) -> Result<Vec<Self>, String> {
        git_member::members::load_all(repo).map_err(|err| err.to_string())
    }
}

impl WebComponent for Member {
    const TITLE: &'static str = "Members";

    fn empty() -> Markup {
        html! {
            div.card-row.muted { "No members — pushes are open until the first key is added." }
        }
    }
}

/// A run's per-check outcomes, each colored by [`status_badge`] and linked to
/// its recording when it has one — the same treatment "Checks on HEAD" gives
/// each check, rather than one plain summary line.
pub(super) fn run_row(rel: &str, commit_hex: &str, run: &Run) -> Markup {
    html! {
        div.run-results {
            @for outcome in &run.results {
                (run_result(rel, commit_hex, outcome))
            }
        }
    }
}

/// One outcome within a run row: its check name and status badge, linked to
/// `/{rel}/checks/{commit_hex}/{name}` when there's a live view or a
/// recording behind it.
fn run_result(rel: &str, commit_hex: &str, outcome: &RunOutcome) -> Markup {
    let href = format!("/{rel}/checks/{commit_hex}/{}", outcome.name);
    let duration = html! {
        @if let Some(secs) = outcome.duration_secs {
            " " span.muted { "(" (secs) "s)" }
        }
    };
    html! {
        @if outcome.recording.is_some() || is_in_progress(outcome.status) {
            a.run-result href=(href) { (outcome.name) " " (status_badge(outcome.status)) }
            (duration)
        } @else {
            span.run-result { (outcome.name) " " (status_badge(outcome.status)) }
            (duration)
        }
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
        Status::Running => "status-running",
        Status::Queued | Status::Skipped => "status-pending",
    };
    html! { span class=(class) { (status.to_string()) } }
}

/// One check's row on the "Checks on HEAD" card: the check's `name` and a
/// status badge, both part of one link to `href` when there's a live view or
/// a recording behind it, or "no run yet" when `outcome` is absent (just
/// added, or its run has not landed).
pub(super) fn check_list_row(name: &str, outcome: Option<&RunOutcome>, href: &str) -> Markup {
    html! {
        @match outcome {
            None => {
                code.key { (name) }
                span.muted { "no run yet" }
            }
            Some(outcome) if outcome.recording.is_some() || is_in_progress(outcome.status) => {
                a.row-link href=(href) {
                    code.key { (name) }
                    (status_badge(outcome.status))
                }
            }
            Some(outcome) => {
                code.key { (name) }
                (status_badge(outcome.status))
            }
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
/// didn't (a composite, or an infra failure before any command started). Boxed
/// distinctly from a real terminal (dashed border, no fixed light background)
/// so it reads as "nothing recorded" rather than as an empty transcript.
fn no_output_notice(outcome: &RunOutcome) -> Markup {
    html! {
        div.terminal-empty {
            @match outcome.exit_code {
                Some(code) => { "Check finished with exit code " code { (code) } " without output." }
                None => { "This check produced no terminal output." }
            }
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
        None => html! { div.terminal-empty { "Waiting for output…" } },
    }
}
