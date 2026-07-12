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
/// (plain text, no `Type::Foo(...)` wrapper -- what a `String` or a
/// `MemberId` newtype gives), and falls back to `Debug` otherwise (every
/// entity struct in `ents-model`/`ents-forge`/`ents-kiln` derives `Debug`,
/// so an enum field like [`ents_model::MemberState`] still renders its
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
/// let member = Member::new("ssh-ed25519 AAAA... jdc", Provenance::AdminRegistered);
/// let rows = ents_web::render::fields(&member);
/// assert_eq!(rows[0].0, "key");
/// assert!(rows[0].1.contains("ssh-ed25519"));
/// assert_eq!(rows[1].0, "state");
/// assert_eq!(rows[1].1, "Active");
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
/// from the entity's own reflected field names -- the generic "list" page
/// every kernel entity this crate exposes uses.
///
/// `id_header` names the leading column holding each entry's key (a
/// username, an effect name, a redaction id -- whatever names the ref this
/// listing was read from, which is never itself a field on the entity).
///
/// # Examples
///
/// ```
/// use ents_model::{Member, Provenance};
///
/// let rows = vec![("jdc".to_owned(), Member::new("key-a", Provenance::AdminRegistered))];
/// let markup = ents_web::render::list_table(&rows, "username", |id| format!("/members/{id}"));
/// assert!(markup.into_string().contains("jdc"));
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
                                td { (rendered) }
                            }
                        }
                    }
                }
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
        let member = Member::new("ssh-ed25519 AAAA... jdc", Provenance::AdminRegistered);
        let rows = fields(&member);
        assert_eq!(
            rows.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
            vec!["key", "state", "provenance"]
        );
    }

    #[rstest]
    // @relation(roots.web-agnostic, scope=function, role=Verifies)
    fn an_enum_field_renders_its_variant_name_not_a_placeholder() {
        let member = Member::new("key", Provenance::AdminRegistered);
        let rows = fields(&member);
        let (_, state) = rows
            .iter()
            .find(|(name, _)| *name == "state")
            .expect("state field");
        assert_eq!(state, "Active");
        assert_eq!(member.state, MemberState::Active);
    }

    #[rstest]
    #[case::member(Member::new("k", Provenance::AdminRegistered))]
    // @relation(roots.web-agnostic, scope=function, role=Verifies)
    fn the_same_generic_view_renders_every_entity_type(#[case] member: Member) {
        // Same call, no type-specific branch -- this is the whole point of
        // the generic mechanism this module exists to prove. Each call's
        // markup is asserted non-empty and containing a field name real to
        // that entity, so this is a render check, not a discarded call.
        assert!(view(&member).into_string().contains("provenance"));
        assert!(
            view(&Effect {
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
            Member::new("key", Provenance::AdminRegistered),
        )];
        let markup = list_table(&rows, "username", |id| format!("/members/{id}")).into_string();
        assert!(markup.contains("username"));
        assert!(markup.contains("key"));
        assert!(markup.contains("jdc"));
    }
}
