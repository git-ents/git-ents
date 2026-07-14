//! `GET /members`, `GET /members/{username}`: the member surface -- an
//! identity card per enrolled key rather than [`crate::render`]'s generic
//! table (an SSH public key's base64 body defeats a table cell; the card
//! shows the key type as a badge and the material truncated through the
//! middle, with the full line behind a `<details>` toggle). Read-only in
//! this phase (enrollment stays a `git ents members add` operation; see
//! this crate's own top-level doc for why write flows are demonstrated on
//! [`super::account`] rather than duplicated per entity).

use std::sync::Arc;

use axum::extract::{Path, State};
use ents_model::{Member, MemberState, Provenance};
use gix_object::{Find, Write};
use maud::{Markup, html};

use crate::error::{Error, Result};
use crate::state::AppState;

/// `GET /members`.
///
/// # Errors
///
/// Propagates a ref-store or object read failure.
pub async fn list<O>(State(state): State<Arc<AppState<O>>>) -> Result<maud::Markup>
where
    O: Find + Write + Send + 'static,
{
    let mut rows = Vec::new();
    let mut failures = Vec::new();
    for (username, member) in read_all(&state)? {
        match member {
            Ok(member) => rows.push((username, member)),
            Err(error) => failures.push((format!("refs/meta/member/{username}"), error)),
        }
    }
    let body = if rows.is_empty() {
        super::blankslate(
            "No members yet",
            maud::html! { "Enroll one with " code { "git ents members add" } "." },
        )
    } else {
        html! {
            @for (username, member) in &rows {
                (member_card(username, member, true))
            }
        }
    };
    Ok(super::layout_meta(
        &super::RepoHeader::from_state(&state),
        &super::identity_label(&state),
        "/members",
        "Members",
        maud::html! {
            (crate::render::unreadable_disclosure(&failures))
            (body)
        },
    ))
}

/// `GET /members/{username}`.
///
/// # Errors
///
/// [`Error::NotFound`] if `username` has no member ref at all -- a member
/// ref that exists but whose stored tree does not match this build's
/// [`Member`] shape degrades to [`crate::render::unreadable`] instead
/// (`roots.web-agnostic`'s graceful-degradation stance).
pub async fn show<O>(
    State(state): State<Arc<AppState<O>>>,
    Path(username): Path<String>,
) -> Result<maud::Markup>
where
    O: Find + Write + Send + 'static,
{
    let (_, member) = read_all(&state)?
        .into_iter()
        .find(|(name, _)| *name == username)
        .ok_or_else(|| Error::NotFound {
            what: format!("member {username}"),
        })?;
    let body = match member {
        Ok(member) => member_card(&username, &member, false),
        Err(detail) => crate::render::unreadable(&detail),
    };
    Ok(super::layout_meta(
        &super::RepoHeader::from_state(&state),
        &super::identity_label(&state),
        "/members",
        &username,
        maud::html! {
            (super::child_crumbs("members", "/members", &username))
            (body)
        },
    ))
}

/// One member's identity card: the username prominent (a link on the list
/// page, plain on the member's own page), the key type as a badge, the
/// state and provenance as muted badges, and the key material truncated
/// through the middle ([`truncate_middle`]) with the full key line behind
/// a `<details>` toggle -- no digest dependency, so no fingerprint; the
/// truncated material plus the expandable full line is the identity a
/// reader compares.
fn member_card(username: &str, member: &Member, link: bool) -> Markup {
    let (key_type, material) = split_key(&member.key);
    html! {
        div.card.member-card {
            div.member-head {
                @if link {
                    a.member-name href={ "/members/" (username) } { (username) }
                } @else {
                    span.member-name { (username) }
                }
                @if let Some(key_type) = key_type {
                    span.key-badge { (key_type) }
                }
                span.badge { (state_label(member.state)) }
                span.badge { (provenance_label(member.provenance)) }
            }
            div.member-key {
                code { (truncate_middle(material)) }
                details {
                    summary { "full key" }
                    pre { (member.key) }
                }
            }
        }
    }
}

/// A member's key line split into its type token (`ssh-ed25519`, ...) and
/// key material -- `(None, whole line)` when the line has no second token
/// to badge (`ents-model` treats the key as opaque text, so this only ever
/// assumes the OpenSSH `type material [comment]` shape when it actually
/// sees one).
fn split_key(key: &str) -> (Option<&str>, &str) {
    let mut parts = key.split_whitespace();
    let first = parts.next().unwrap_or("");
    match parts.next() {
        Some(material) => (Some(first), material),
        None => (None, first),
    }
}

/// Key material truncated through the middle (`AAAA…zM7f`), leaving the
/// start and end a reader actually compares -- the full line stays one
/// `<details>` toggle away.
fn truncate_middle(material: &str) -> String {
    const HEAD: usize = 12;
    const TAIL: usize = 8;
    let count = material.chars().count();
    if count <= HEAD.saturating_add(TAIL).saturating_add(1) {
        return material.to_owned();
    }
    let head: String = material.chars().take(HEAD).collect();
    let tail: String = material.chars().skip(count.saturating_sub(TAIL)).collect();
    format!("{head}\u{2026}{tail}")
}

/// [`MemberState`] as its badge text.
fn state_label(state: MemberState) -> &'static str {
    match state {
        MemberState::Active => "active",
        MemberState::Revoked => "revoked",
    }
}

/// [`Provenance`] as its badge text.
fn provenance_label(provenance: Provenance) -> &'static str {
    match provenance {
        Provenance::AdminRegistered => "admin-registered",
        Provenance::SelfAttested => "self-attested",
    }
}

/// Every `refs/meta/member/*` ref, with its tip's tree deserialized as a
/// [`Member`] -- `Err(detail)` for a ref this build's `#[derive(Facet)]`
/// shape could not read back, kept in the listing (not dropped) so
/// [`list`] can surface it through
/// [`crate::render::unreadable_disclosure`] and [`show`] as
/// [`crate::render::unreadable`]'s marker card, rather than silently
/// omitting it (`roots.web-agnostic`: a reader surfaces a marker, never an
/// error or a silent gap, for one entity written by a schema this build no
/// longer speaks).
fn read_all<O: Find>(
    state: &AppState<O>,
) -> Result<Vec<(String, std::result::Result<Member, String>)>> {
    let mut out = Vec::new();
    for entry in state.refs.iter_prefix("refs/meta/member/")? {
        let (name, tip) = entry?;
        let path = name.as_bstr().to_string();
        let Some(username) = path.strip_prefix("refs/meta/member/") else {
            continue;
        };
        // One `state.objects()` lock per iteration, reused for both reads:
        // `state.objects()` a second time *within the same statement*
        // would try to lock this non-reentrant `Mutex` while the first
        // guard is still alive (a `let`'s temporaries live to its own
        // `;`), self-deadlocking forever rather than erroring.
        let objects = state.objects();
        let member = super::commit_tree(&*objects, tip)
            .map_err(|error| error.to_string())
            .and_then(|tree| {
                facet_git_tree::deserialize::<Member>(&tree, &*objects)
                    .map_err(|error| error.to_string())
            });
        out.push((username.to_owned(), member));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "unit test")]

    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::openssh(
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJq4 jdc@host",
        Some("ssh-ed25519"),
        "AAAAC3NzaC1lZDI1NTE5AAAAIJq4"
    )]
    #[case::bare_token("opaquekeymaterial", None, "opaquekeymaterial")]
    fn split_key_badges_only_a_typed_key_line(
        #[case] key: &str,
        #[case] key_type: Option<&str>,
        #[case] material: &str,
    ) {
        assert_eq!(split_key(key), (key_type, material));
    }

    #[test]
    fn truncate_middle_keeps_the_start_and_end_of_a_long_key() {
        let material = "AAAAC3NzaC1lZDI1NTE5AAAAIJq4rB5zM7f";
        let shown = truncate_middle(material);
        assert!(shown.starts_with("AAAAC3NzaC1l"));
        assert!(shown.ends_with("rB5zM7f"));
        assert!(shown.contains('\u{2026}'));
        assert_eq!(
            truncate_middle("short"),
            "short",
            "a short token is left whole"
        );
    }
}
