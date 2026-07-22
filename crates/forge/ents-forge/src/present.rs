//! Schema-driven entity output: one reflection walk over any
//! `#[derive(Facet)]` entity's [`facet::Shape`], presentation policy read
//! from the `ents` attributes declared on the entity's own fields
//! ([`ents_attrs::Attr`]) — never from a branch on the concrete entity
//! type. The CLI derives its `show` lines, `list` columns, and porcelain
//! records here; a surface that genuinely needs a domain-specific line (a
//! comment's projected anchor, a review's thread) appends it beside this
//! module's output rather than reaching into the walk.

use facet::{Facet, Field, Peek};

/// One rendered field: its declared name and display value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldLine {
    /// The field's declared name, exactly as the struct spells it.
    pub name: &'static str,
    /// The field's rendered value.
    pub value: String,
}

/// An entity's `show` view: `field: value` lines in declaration order,
/// with the `ents::body`-marked field split out so a caller can interleave
/// domain-specific lines before it. Its `Display` renders lines then body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    /// Every non-skipped, non-body field's line, in declaration order.
    pub lines: Vec<FieldLine>,
    /// The `ents::body` field's line, rendered last.
    pub body: Option<FieldLine>,
}

impl std::fmt::Display for View {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for line in self.lines.iter().chain(&self.body) {
            writeln!(f, "{}: {}", line.name, line.value)?;
        }
        Ok(())
    }
}

/// Reflect `value` into its human `show` view: one `field: value` line per
/// non-skipped field, empties omitted where `ents::skip_empty` says so,
/// id-valued fields abbreviated, the `ents::body` field split out last.
///
/// # Examples
///
/// ```
/// let issue = ents_forge::Issue {
///     title: "gate rejects a valid signature".to_owned(),
///     body: "steps to reproduce...".to_owned(),
///     state: "open".to_owned(),
///     assignees: vec![],
///     labels: vec!["bug".to_owned(), "gate".to_owned()],
/// };
/// let rendered = ents_forge::present::view(&issue).to_string();
/// assert_eq!(
///     rendered,
///     "title: gate rejects a valid signature\nstate: open\nlabels: bug, gate\nbody: steps to reproduce...\n"
/// );
/// ```
#[must_use]
pub fn view<T: Facet<'static>>(value: &T) -> View {
    let mut lines = Vec::new();
    let mut body = None;
    for row in rows(value, Audience::Human) {
        if row.policy.skip_empty && row.empty {
            continue;
        }
        let line = FieldLine {
            name: row.name,
            value: row.value,
        };
        if row.policy.body {
            body = Some(line);
        } else {
            lines.push(line);
        }
    }
    View { lines, body }
}

/// Reflect `value` into its human `list` columns: `ents::head` fields
/// first, then `ents::col` fields, each set in declaration order,
/// id-valued fields abbreviated. The caller prepends the row's own id
/// column(s) and joins with tabs.
///
/// # Examples
///
/// ```
/// let issue = ents_forge::Issue {
///     title: "gate rejects a valid signature".to_owned(),
///     body: String::new(),
///     state: "open".to_owned(),
///     assignees: vec![],
///     labels: vec![],
/// };
/// assert_eq!(
///     ents_forge::present::columns(&issue),
///     vec!["open".to_owned(), "gate rejects a valid signature".to_owned()]
/// );
/// ```
#[must_use]
pub fn columns<T: Facet<'static>>(value: &T) -> Vec<String> {
    let rows = rows(value, Audience::Human);
    let heads = rows.iter().filter(|row| row.policy.head);
    let cols = rows.iter().filter(|row| row.policy.col && !row.policy.head);
    heads.chain(cols).map(|row| row.value.clone()).collect()
}

/// Reflect `value` into one porcelain record (`lens.parity`), the record
/// grammar `git ents comment list --porcelain` established: a head line of
/// `id` then each `ents::head` field's value, space-separated; one
/// `<name> <value>` line per remaining field (omitted when
/// `ents::skip_empty` and empty); the `ents::body` field's lines each
/// tab-prefixed. Ids render full, never abbreviated.
///
/// # Examples
///
/// ```
/// let effect = ents_model::Effect {
///     name: "unit".to_owned(),
///     trigger: "rev(refs/heads/main)".to_owned(),
///     toolchains: vec![],
///     run: "cargo test".to_owned(),
/// };
/// assert_eq!(
///     ents_forge::present::record("unit", &effect),
///     "unit\ntrigger rev(refs/heads/main)\nrun cargo test\n"
/// );
/// ```
#[must_use]
pub fn record<T: Facet<'static>>(id: &str, value: &T) -> String {
    let rows = rows(value, Audience::Porcelain);
    let mut out = id.to_owned();
    for row in rows.iter().filter(|row| row.policy.head) {
        out.push(' ');
        out.push_str(&row.value);
    }
    out.push('\n');
    for row in &rows {
        if row.policy.head || row.policy.body || (row.policy.skip_empty && row.empty) {
            continue;
        }
        out.push_str(row.name);
        out.push(' ');
        out.push_str(&row.value);
        out.push('\n');
    }
    if let Some(body) = rows.iter().find(|row| row.policy.body) {
        for line in body.value.lines() {
            out.push('\t');
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// [`record`] over every `(id, entity)` row, records separated by one
/// blank line — the whole `--porcelain` output for a listing.
#[must_use]
pub fn porcelain<T: Facet<'static>>(rows: &[(String, T)]) -> String {
    rows.iter()
        .map(|(id, value)| record(id, value))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Who the rendering is for: humans get abbreviated ids, porcelain full.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Audience {
    Human,
    Porcelain,
}

/// The presentation roles one field declares via `#[facet(ents::...)]` —
/// the parsed form of [`ents_attrs::Attr`], read once per field.
#[derive(Default, Clone, Copy)]
struct FieldPolicy {
    skip: bool,
    head: bool,
    col: bool,
    skip_empty: bool,
    id: bool,
    body: bool,
}

impl FieldPolicy {
    fn of(field: &Field) -> Self {
        let has = |key: &str| field.has_attr(Some("ents"), key);
        Self {
            skip: has("skip"),
            head: has("head"),
            col: has("col"),
            skip_empty: has("skip_empty"),
            id: has("id"),
            body: has("body"),
        }
    }
}

/// One walked field: its policy, name, rendered value, and emptiness.
struct Row {
    policy: FieldPolicy,
    name: &'static str,
    value: String,
    empty: bool,
}

/// Walk `value`'s shape into one [`Row`] per non-`ents::skip` field, in
/// declaration order. A non-struct `T` yields no rows — reflection is a
/// presentation convenience, never a correctness path.
fn rows<T: Facet<'static>>(value: &T, audience: Audience) -> Vec<Row> {
    let peek = Peek::new(value);
    let Ok(structure) = peek.into_struct() else {
        return Vec::new();
    };
    structure
        .ty()
        .fields
        .iter()
        .enumerate()
        .filter_map(|(index, field)| {
            let policy = FieldPolicy::of(field);
            if policy.skip {
                return None;
            }
            let peek = structure.field(index).ok()?;
            Some(Row {
                policy,
                name: field.name,
                value: render(peek, policy, audience),
                empty: is_empty(peek),
            })
        })
        .collect()
}

/// Render one field's value: id-valued fields as full-or-abbreviated ids
/// (raw 20-byte oids as hex), otherwise [`scalar`].
fn render(peek: Peek<'_, '_>, policy: FieldPolicy, audience: Audience) -> String {
    if !policy.id {
        return scalar(peek);
    }
    let full = peek
        .get::<[u8; 20]>()
        .map(|bytes| bytes.iter().map(|byte| format!("{byte:02x}")).collect())
        .unwrap_or_else(|_| scalar(peek));
    match audience {
        Audience::Human => crate::abbreviate_id(&full).to_owned(),
        Audience::Porcelain => full,
    }
}

/// Render one value as plain text: a `str` verbatim, an `Option` as its
/// inner value (or empty), a list as its items joined with `", "`, and
/// anything else via its own `Display` — falling back to `Debug` so an
/// enum without `Display` still shows its variant name rather than an
/// opaque placeholder (the same rule `ents-web`'s renderer applies).
fn scalar(peek: Peek<'_, '_>) -> String {
    if let Some(text) = peek.as_str() {
        return text.to_owned();
    }
    if let Ok(option) = peek.into_option() {
        return option.value().map(scalar).unwrap_or_default();
    }
    if let Ok(list) = peek.into_list_like() {
        return list.iter().map(scalar).collect::<Vec<_>>().join(", ");
    }
    let displayed = format!("{peek}");
    if displayed.starts_with('\u{27e8}') {
        format!("{peek:?}")
    } else {
        displayed
    }
}

/// Whether a value counts as empty for `ents::skip_empty`: an empty
/// string, a `None`, or an empty list.
fn is_empty(peek: Peek<'_, '_>) -> bool {
    if let Some(text) = peek.as_str() {
        return text.is_empty();
    }
    if let Ok(option) = peek.into_option() {
        return option.is_none();
    }
    if let Ok(list) = peek.into_list_like() {
        return list.is_empty();
    }
    false
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "unit test")]

    use ents_model::MemberId;
    use gix_hash::ObjectId;
    use rstest::rstest;

    use super::*;
    use crate::Issue;
    use crate::comment::Comment;
    use crate::review::{Review, Verdict};

    fn issue() -> Issue {
        Issue {
            title: "gate rejects a valid signature".to_owned(),
            body: "first line\n\nthird line".to_owned(),
            state: "open".to_owned(),
            assignees: vec![MemberId::new("jdc"), MemberId::new("alice")],
            labels: vec![],
        }
    }

    fn review() -> Review {
        let target =
            ObjectId::from_hex(b"0123456789abcdef0123456789abcdef01234567").expect("valid hex");
        Review::new(target, Verdict::RequestChanges, "please fix")
    }

    /// The whole walk is attribute-driven: the same [`view`] call renders
    /// an issue and a comment with each one's own field policy, no branch
    /// on the concrete type anywhere in this module.
    #[rstest]
    // @relation(model.issue, model.comment, scope=function, role=Verifies)
    fn view_orders_lines_by_declaration_with_the_body_last_and_empties_skipped() {
        let rendered = view(&issue()).to_string();
        assert_eq!(
            rendered,
            "title: gate rejects a valid signature\nstate: open\nassignees: jdc, alice\nbody: first line\n\nthird line\n"
        );

        let comment = Comment {
            body: "looks off".to_owned(),
            state: "open".to_owned(),
            anchor: None,
            context: None,
            parent: Some("0123456789abcdef0123456789abcdef01234567".to_owned()),
        };
        assert_eq!(
            view(&comment).to_string(),
            "state: open\nparent: 0123456\nbody: looks off\n"
        );
    }

    /// `model.issue`: human columns abbreviate id-valued fields the way
    /// git abbreviates oids; head columns lead, then plain columns.
    #[rstest]
    // @relation(model.issue, model.review, scope=function, role=Verifies)
    fn columns_lead_with_head_fields_and_abbreviate_ids() {
        assert_eq!(
            columns(&issue()),
            vec![
                "open".to_owned(),
                "gate rejects a valid signature".to_owned()
            ]
        );
        assert_eq!(
            columns(&review()),
            vec![
                "0123456".to_owned(),
                "request-changes".to_owned(),
                "active".to_owned()
            ]
        );
    }

    /// `lens.parity`, `model.issue`: a porcelain record carries the full
    /// id and full field values on a space-separated head line, keyed
    /// lines for the rest, and the body tab-prefixed line by line.
    #[rstest]
    // @relation(lens.parity, model.issue, model.review, scope=function, role=Verifies)
    fn record_renders_full_ids_keyed_lines_and_a_tab_prefixed_body() {
        let id = "89abcdef0123456789abcdef0123456789abcdef";
        assert_eq!(
            record(id, &issue()),
            format!(
                "{id} open\ntitle gate rejects a valid signature\nassignees jdc, alice\n\tfirst line\n\t\n\tthird line\n"
            )
        );
        assert_eq!(
            record("0123456789abcdef0123456789abcdef01234567 jdc", &review()),
            "0123456789abcdef0123456789abcdef01234567 jdc \
             0123456789abcdef0123456789abcdef01234567 request-changes active\n\tplease fix\n"
        );
    }

    /// Records separate with exactly one blank line, mirroring the comment
    /// porcelain grammar.
    #[rstest]
    // @relation(lens.parity, scope=function, role=Verifies)
    fn porcelain_separates_records_with_one_blank_line() {
        let rows = vec![
            ("a".repeat(40), issue()),
            ("b".repeat(40), issue()),
        ];
        let rendered = porcelain(&rows);
        assert_eq!(rendered.split("\n\n").count(), 2, "{rendered}");
        assert!(
            rendered.contains(&format!("\tthird line\n\n{}", "b".repeat(40))),
            "a blank body line renders as a lone tab, so only the record \
             separator is a true blank line: {rendered}"
        );
        assert!(rendered.ends_with("third line\n"));
    }

    /// A non-struct value yields no rows, never a panic — reflection is a
    /// presentation convenience, not a correctness path.
    #[rstest]
    fn a_non_struct_value_renders_as_nothing() {
        let empty = view(&42u32);
        assert!(empty.lines.is_empty() && empty.body.is_none());
        assert!(columns(&42u32).is_empty());
    }
}
