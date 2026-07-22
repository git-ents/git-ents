//! The generic, schema-driven list/view rendering mechanism -- the UI
//! analog of the gate executor named in this crate's development-plan
//! row: one reflection walk over any `#[derive(Facet)]` entity's
//! [`facet::Shape`], reused for every kernel entity this crate lists or
//! shows, rather than one hand-written renderer per entity type.
//!
//! The binding rule this module exists to uphold: nothing here ever
//! matches on *which* concrete type it was handed. [`fields`] walks
//! whatever [`facet::Shape`] the type reflects, by field name and
//! position, exactly the same way for [`ents_model::Member`],
//! [`ents_model::Effect`], [`ents_model::Redaction`], or
//! [`ents_model::Account`]. A page that genuinely needs to know it is
//! showing a comment (to render an anchor's projected diff) or a
//! toolchain (to render a recipe's provenance) is not a gap in this
//! module -- it is [`crate::pages::comments`] or [`crate::pages::toolchains`]
//! choosing a legitimate custom view instead of this generic one, exactly
//! as this crate's development-plan row anticipates.

use facet::Facet;
use maud::{Markup, html};

/// One field's name and rendered value, in declaration order.
pub type FieldRow = (&'static str, String);

/// Reflect over `value`'s [`facet::Shape`] and return one `(name, value)`
/// pair per field, in declaration order.
///
/// A field's value renders via its own `Display` impl when it has one
/// (plain text, no `Type::Foo(...)` wrapper -- what a `String`, a
/// `MemberId` newtype, or an enum like [`ents_model::MemberState`] with its
/// own canonical `Display` gives), and falls back to `Debug` otherwise
/// (every entity struct in `ents-model`/`ents-forge`/`ents-kiln` derives
/// `Debug`, so a field of an enum with no `Display` still renders its
/// variant name rather than an opaque placeholder). A field this crate
/// cannot even walk as a struct (called on a non-struct `T`) renders as an
/// empty list, not a panic -- reflection is a UI convenience, never a
/// correctness path.
///
/// # Examples
///
/// ```
/// use ents_model::{Member, Provenance};
///
/// let member = Member::new("jdc", "ssh-ed25519 AAAA... jdc", Provenance::AdminRegistered);
/// let rows = ents_web::render::fields(&member);
/// assert_eq!(rows[0].0, "id");
/// assert_eq!(rows[1].0, "key");
/// assert!(rows[1].1.contains("ssh-ed25519"));
/// assert_eq!(rows[2].0, "state");
/// assert_eq!(rows[2].1, "active");
/// ```
#[must_use]
pub fn fields<T: Facet<'static>>(value: &T) -> Vec<FieldRow> {
    let peek = facet_reflect::Peek::new(value);
    let Ok(structure) = peek.into_struct() else {
        return Vec::new();
    };
    structure
        .ty()
        .fields
        .iter()
        .enumerate()
        .map(|(index, field)| {
            let rendered = structure
                .field(index)
                .map(render_scalar)
                .unwrap_or_default();
            (field.name, rendered)
        })
        .collect()
}

/// Render one field's [`facet_reflect::Peek`] as plain text: its own
/// `Display` if it has one, else `Debug`, so an enum still shows a variant
/// name instead of this crate's opaque `⟨TypeName⟩` placeholder.
fn render_scalar(peek: facet_reflect::Peek<'_, '_>) -> String {
    if let Some(s) = peek.as_str() {
        return s.to_owned();
    }
    let displayed = format!("{peek}");
    if displayed.starts_with('⟨') {
        format!("{peek:?}")
    } else {
        displayed
    }
}

/// A definition-list view of one entity's fields -- the generic "show"
/// page every kernel entity this crate exposes uses.
///
/// # Examples
///
/// ```
/// use ents_model::{Account, MemberId};
///
/// let account = Account { member: MemberId::new("jdc"), login: "jdc@ents.test".to_owned() };
/// let markup = ents_web::render::view(&account);
/// assert!(markup.into_string().contains("login"));
/// ```
#[must_use]
pub fn view<T: Facet<'static>>(value: &T) -> Markup {
    let rows = fields(value);
    html! {
        div.card {
            dl.entity-view {
                @for (name, rendered) in &rows {
                    dt { (name) }
                    dd { (rendered) }
                }
            }
        }
    }
}

/// A table listing `rows`, one row per `(id, entity)` pair, columns taken
/// from the first entity's own reflected field names -- the generic
/// "list" page every kernel entity this crate exposes uses.
///
/// `id_header` names the leading column holding each entry's key (a
/// username, an effect name, a redaction id -- whatever names the ref this
/// listing was read from, which is never itself a field on the entity).
///
/// Rows are the readable entities only: a ref whose stored tree this
/// build's `#[derive(Facet)]` shape could not read back is not this
/// table's row to render -- the page surfaces it through
/// [`unreadable_disclosure`] beside this table instead (the one place
/// unreadable entities render, for every family alike), and its own show
/// page still renders [`unreadable`]'s marker card.
///
/// # Examples
///
/// ```
/// use ents_model::{Member, Provenance};
///
/// let rows = vec![
///     ("jdc".to_owned(), Member::new("jdc", "key-a", Provenance::AdminRegistered)),
/// ];
/// let rendered = ents_web::render::list_table(&rows, "username", |id| format!("/members/{id}")).into_string();
/// assert!(rendered.contains("jdc"));
/// assert!(rendered.contains("key-a"));
/// ```
#[must_use]
pub fn list_table<T: Facet<'static>>(
    rows: &[(String, T)],
    id_header: &str,
    href_for: impl Fn(&str) -> String,
) -> Markup {
    let field_names: Vec<&'static str> = rows
        .first()
        .map(|(_, entity)| fields(entity).into_iter().map(|(name, _)| name).collect())
        .unwrap_or_default();
    html! {
        div.card {
            table.entity-list {
                thead {
                    tr {
                        th { (id_header) }
                        @for name in &field_names {
                            th { (name) }
                        }
                    }
                }
                tbody {
                    @for (id, entity) in rows {
                        tr {
                            td { a href=(href_for(id)) { (id) } }
                            @for (_, rendered) in fields(entity) {
                                td.long-token[has_long_token(&rendered)] { (rendered) }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Whether `value` holds a token no wrap opportunity ever splits -- an ssh
/// key's base64 body, an unbroken hash -- long enough (over 40 characters)
/// that its cell must be allowed to break mid-token (`.long-token`'s
/// `break-all`) or it starves every other column of the table's width.
/// Ordinary short values keep word-boundary wrapping so a variant name
/// like `AdminRegistered` never shreds.
fn has_long_token(value: &str) -> bool {
    value.split_whitespace().any(|token| token.len() > 40)
}

/// A muted marker card for one entity this crate could not reflect -- the
/// `GET /{family}/{id}` show-page counterpart to [`list_table`]'s per-row
/// marker: the same "unreadable" note, plus `detail` (the underlying
/// deserialization error) rendered verbatim in muted monospace, so an
/// operator can diagnose the schema mismatch without leaving the browser.
/// Never a 500 -- reading an older or unrelated schema's tree degrades to
/// this card, exactly as [`list_table`] degrades one row of a listing.
///
/// # Examples
///
/// ```
/// let rendered = ents_web::render::unreadable("object ... is not a blob").into_string();
/// assert!(rendered.contains("unreadable"));
/// assert!(rendered.contains("is not a blob"));
/// ```
#[must_use]
pub fn unreadable(detail: &str) -> Markup {
    html! {
        div.card {
            div.card-row.unreadable {
                span { "unreadable \u{2014} written by an older schema" }
            }
            div.card-row {
                code.unreadable-detail { (detail) }
            }
        }
    }
}

/// The subtle "this page has unreadable entities" disclosure a list page
/// renders when one or more refs under its prefix failed to read back as
/// this build's entity shape: a muted `<details>` badge ("N unreadable",
/// warning glyph) that expands -- no JS, just the element's own toggle --
/// to a small card listing each failed refname and its error text. One
/// component for every entity family (members, effects, redactions,
/// toolchains, comments, issues), so unreadable entities are surfaced the
/// same way everywhere instead of a per-page mix of inline rows and
/// silent gaps. Renders nothing at all when `items` is empty, so a
/// healthy page carries no extra markup.
///
/// # Examples
///
/// ```
/// let items = vec![(
///     "refs/meta/comments/legacy".to_owned(),
///     "object ... is not a blob".to_owned(),
/// )];
/// let rendered = ents_web::render::unreadable_disclosure(&items).into_string();
/// assert!(rendered.contains("<details"));
/// assert!(rendered.contains("1 unreadable"));
/// assert!(rendered.contains("refs/meta/comments/legacy"));
/// assert!(ents_web::render::unreadable_disclosure(&[]).into_string().is_empty());
/// ```
#[must_use]
pub fn unreadable_disclosure(items: &[(String, String)]) -> Markup {
    if items.is_empty() {
        return html! {};
    }
    html! {
        details.unreadable-note {
            summary {
                "\u{26a0} " (items.len()) " unreadable"
            }
            div.card {
                dl.entity-view {
                    @for (refname, error) in items {
                        dt { (refname) }
                        dd { (error) }
                    }
                }
            }
        }
    }
}

/// A key-value properties table for a rendered document's own metadata --
/// Markdown frontmatter ([`crate::markdown`]) and an AsciiDoc header's
/// attribute entries ([`crate::asciidoc`]) both render through this one
/// component, above the document body, styled by `ents.css`'s
/// `.doc-props` rules on top of the same `.entity-view` definition-list
/// look every generic entity view already has. Values are plain text
/// (maud-escaped as any interpolation is); a nested structure the caller
/// chose not to parse arrives here as its raw text and renders verbatim
/// (`.doc-props dd` preserves its line breaks). Renders nothing at all
/// when `entries` is empty, so a document with no metadata carries no
/// empty table.
///
/// # Examples
///
/// ```
/// let entries = vec![("title".to_owned(), "Design Notes".to_owned())];
/// let rendered = ents_web::render::properties_table(&entries).into_string();
/// assert!(rendered.contains("doc-props"));
/// assert!(rendered.contains("Design Notes"));
/// assert!(ents_web::render::properties_table(&[]).into_string().is_empty());
/// ```
#[must_use]
pub fn properties_table(entries: &[(String, String)]) -> Markup {
    if entries.is_empty() {
        return html! {};
    }
    html! {
        dl.entity-view.doc-props {
            @for (key, value) in entries {
                dt { (key) }
                dd { (value) }
            }
        }
    }
}

/// A list of plain strings with no reflected entity behind them (inbox
/// entries, toolchain names) -- deliberately not the [`fields`] mechanism,
/// since there is no struct to reflect over, only a bare list of ids.
#[must_use]
pub fn string_list(rows: &[String], href_for: impl Fn(&str) -> String) -> Markup {
    html! {
        div.card {
            ul.string-list {
                @for row in rows {
                    li { a href=(href_for(row)) { (row) } }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "unit test")]

    use ents_model::{Account, Effect, Member, MemberId, MemberState, Provenance, Redaction};
    use rstest::rstest;

    use super::*;

    #[rstest]
    // @relation(roots.web-agnostic, scope=function, role=Verifies)
    fn fields_walks_every_declared_field_in_order_for_any_kernel_entity() {
        let member = Member::new(
            "jdc",
            "ssh-ed25519 AAAA... jdc",
            Provenance::AdminRegistered,
        );
        let rows = fields(&member);
        assert_eq!(
            rows.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
            vec!["id", "key", "state", "provenance"]
        );
    }

    #[rstest]
    // @relation(roots.web-agnostic, scope=function, role=Verifies)
    fn an_enum_field_renders_its_variant_name_not_a_placeholder() {
        let member = Member::new("jdc", "key", Provenance::AdminRegistered);
        let rows = fields(&member);
        let (_, state) = rows
            .iter()
            .find(|(name, _)| *name == "state")
            .expect("state field");
        assert_eq!(state, "active");
        assert_eq!(member.state, MemberState::Active);
    }

    #[rstest]
    #[case::member(Member::new("jdc", "k", Provenance::AdminRegistered))]
    // @relation(roots.web-agnostic, scope=function, role=Verifies)
    fn the_same_generic_view_renders_every_entity_type(#[case] member: Member) {
        // Same call, no type-specific branch -- this is the whole point of
        // the generic mechanism this module exists to prove. Each call's
        // markup is asserted non-empty and containing a field name real to
        // that entity, so this is a render check, not a discarded call.
        assert!(view(&member).into_string().contains("provenance"));
        assert!(
            view(&Effect {
                name: "unit".to_owned(),
                trigger: "rev(refs/heads/main)".to_owned(),
                toolchains: vec![],
                run: "true".to_owned(),
            })
            .into_string()
            .contains("trigger")
        );
        assert!(
            view(&Redaction::new(
                gix_hash::ObjectId::null(gix_hash::Kind::Sha1),
                "why"
            ))
            .into_string()
            .contains("reason")
        );
        assert!(
            view(&Account {
                member: MemberId::new("jdc"),
                login: "jdc@ents.test".to_owned(),
            })
            .into_string()
            .contains("login")
        );
    }

    #[rstest]
    // @relation(roots.web-agnostic, scope=function, role=Verifies)
    fn list_table_derives_its_columns_from_the_first_rows_own_shape() {
        let rows = vec![(
            "jdc".to_owned(),
            Member::new("jdc", "key", Provenance::AdminRegistered),
        )];
        let markup = list_table(&rows, "username", |id| format!("/members/{id}")).into_string();
        assert!(markup.contains("username"));
        assert!(markup.contains("key"));
        assert!(markup.contains("jdc"));
    }

    #[rstest]
    // @relation(roots.web-agnostic, scope=function, role=Verifies)
    fn list_table_breaks_only_the_cell_holding_a_long_unbroken_token() {
        let key = format!("ssh-ed25519 {} jdc@host", "A".repeat(68));
        let rows = vec![(
            "jdc".to_owned(),
            Member::new("jdc", key, Provenance::AdminRegistered),
        )];
        let markup = list_table(&rows, "username", |id| format!("/members/{id}")).into_string();
        // Exactly one cell carries the class: the key's; `AdminRegistered`
        // and the short id stay word-boundary-wrapped.
        assert_eq!(markup.matches("class=\"long-token\"").count(), 1);
    }

    #[rstest]
    // @relation(roots.web-agnostic, scope=function, role=Verifies)
    fn unreadable_disclosure_lists_each_failed_ref_behind_a_details_toggle() {
        let items = vec![
            (
                "refs/meta/member/legacy".to_owned(),
                "object ... is not a blob".to_owned(),
            ),
            (
                "refs/meta/member/older".to_owned(),
                "missing field".to_owned(),
            ),
        ];
        let markup = unreadable_disclosure(&items).into_string();
        assert!(markup.contains("<details"));
        assert!(markup.contains("2 unreadable"));
        assert!(markup.contains("refs/meta/member/legacy"));
        assert!(markup.contains("missing field"));
        assert!(
            unreadable_disclosure(&[]).into_string().is_empty(),
            "a healthy page carries no disclosure at all"
        );
    }

    #[rstest]
    // @relation(roots.web-agnostic, scope=function, role=Verifies)
    fn unreadable_card_shows_the_underlying_error() {
        let markup = unreadable("object deadbeef is not a blob").into_string();
        assert!(markup.contains("unreadable"));
        assert!(markup.contains("object deadbeef is not a blob"));
    }
}
