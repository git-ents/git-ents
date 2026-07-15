//! `GET /account`, `POST /account`: who the current session is (the
//! serving identity's enrolled member, `roots.web-signing` -- there is no
//! login flow, the signing key *is* the identity), followed by the
//! generic *view* of [`ents_model::Account`] (`crate::render::view`,
//! reflection-driven, the same mechanism [`super::members`] and
//! [`super::redactions`] use), paired with this crate's one demonstrated
//! generic-edit write flow (`roots.web-session`'s signed, CSRF-checked
//! mutation path).
//!
//! Account is the write-flow demo rather than every entity because it is
//! the simplest possible case -- two string-shaped fields, one fixed ref,
//! no anchor or recipe machinery to special-case -- so the CSRF/session/
//! signing plumbing this page exercises is visible without also chasing a
//! more complex entity's own domain logic. Every other write flow this
//! crate ships ([`super::comments::add`]) is a legitimate custom page for
//! exactly the reason `ents-forge`'s own comment command is: anchoring
//! needs a repository checkout and a projection, not a bare form.

use std::sync::Arc;

use axum::Form;
use axum::extract::State;
use axum::response::{IntoResponse, Redirect};
use ents_model::{Account, Member, MemberId, namespace};
use ents_receive::propose_entity;
use gix_object::{Find, Write};
use maud::html;
use serde::Deserialize;

use crate::error::{Error, Result};
use crate::session::Session;
use crate::state::AppState;

/// `GET /account`: who the current session is, first -- the enrolled
/// member whose key the serving identity signs with, as the same identity
/// card `crate::pages::members` renders, or the unenrolled key itself --
/// then the recorded [`Account`] (the hosted login mapping) with its edit
/// form. There is no login flow to land on: a local root's identity *is*
/// the signing key `git ents serve` resolved at startup
/// (`roots.web-signing`), so this page states that rather than asking for
/// credentials.
///
/// # Errors
///
/// Propagates a ref-store or object read failure.
pub async fn show<O>(
    State(state): State<Arc<AppState<O>>>,
    axum::Extension(session): axum::Extension<Session>,
) -> Result<maud::Markup>
where
    O: Find + Write + Send + 'static,
{
    let pubkey = state.identity.public_openssh();
    let enrolled = resolve_member_by_key(&state, &pubkey).ok();
    let current = read(&state)?;
    let (member_value, login_value) = match &current {
        Some(account) => (account.member.as_str().to_owned(), account.login.clone()),
        None => (String::new(), String::new()),
    };
    let view = current
        .as_ref()
        .map(crate::render::view)
        .unwrap_or_else(|| html! { p.muted { "No login mapping recorded." } });

    Ok(super::layout(
        &super::RepoHeader::from_state(&state),
        &super::identity_label(&state),
        super::Tab::Account,
        "Account",
        html! {
            div.readable {
                @match &enrolled {
                    Some((username, member)) => {
                        p {
                            "Signed in as the member below. Every web edit is a "
                            "mutation commit signed with this key, exactly as "
                            code { "git ents" }
                            " itself would sign it -- a local root has no separate login."
                        }
                        (super::members::member_card(username.as_str(), member, true))
                    }
                    None => {
                        div.card {
                            p {
                                "This signing key is not enrolled as a "
                                a href="/members" { "member" }
                                " of this repository. Edits still sign with it; enroll "
                                "the key to have them attributed to a username."
                            }
                            pre { (pubkey) }
                        }
                    }
                }
                h2 { "Hosted login" }
                p.muted {
                    "A hosted deployment maps an external login to an enrolled "
                    "member so its pushes can be attributed. A local root never "
                    "needs one -- the key above is the identity."
                }
                (view)
                details {
                    summary { "Edit" }
                    form method="post" action="/account" {
                        (super::csrf_input(&session))
                        label { "member" input type="text" name="member" value=(member_value) list="members"; }
                        label { "login" input type="text" name="login" value=(login_value); }
                        button type="submit" { "Save" }
                    }
                    (super::members_datalist(&state))
                }
            }
        },
    ))
}

/// The form fields `POST /account` accepts.
#[derive(Debug, Deserialize)]
pub struct AccountForm {
    /// The member this account belongs to; if blank, resolved from the
    /// signing identity's own enrolled key (mirrors
    /// `git_ents::commands::account::create`'s identical default).
    #[serde(default)]
    member: String,
    /// The login identity to record.
    login: String,
    /// The per-session CSRF token (`roots.web-session`).
    csrf: String,
}

/// `POST /account`: create or update the account, signed
/// (`roots.web-signing`) on behalf of the current session
/// (`roots.web-session`).
///
/// # Errors
///
/// [`Error::BadCsrf`] if `form.csrf` does not match the session's own
/// token; [`Error::NotFound`] if `member` is blank and the signing
/// identity's key is not an enrolled member; otherwise propagates a
/// serialization or `receive` failure.
// @relation(roots.web-signing, roots.web-session, scope=function)
pub async fn update<O>(
    State(state): State<Arc<AppState<O>>>,
    axum::Extension(session): axum::Extension<Session>,
    Form(form): Form<AccountForm>,
) -> Result<impl IntoResponse>
where
    O: Find + Write + Send + 'static,
{
    super::require_csrf(&session, &form.csrf)?;

    let member = if form.member.trim().is_empty() {
        resolve_member_by_key(&state, &state.identity.public_openssh())?.0
    } else {
        MemberId::new(form.member.trim())
    };
    let account = Account {
        member,
        login: form.login,
    };

    #[expect(
        clippy::expect_used,
        reason = "ACCOUNT_REF is a fixed, compile-time-known-valid refname literal, mirroring \
                  git_ents::commands::account's identical unguarded conversion"
    )]
    let name: gix::refs::FullName = namespace::ACCOUNT_REF
        .try_into()
        .expect("fixed, valid refname");

    let identity = state.identity.as_ref();
    let outcome = propose_entity(
        state.refs.as_ref(),
        &*state.objects(),
        state.events.as_ref(),
        name,
        &account,
        &crate::receive_identity!(identity),
        "Create account (web)",
        state.mode,
    )?;
    crate::error::outcome_to_result(outcome)?;
    Ok(Redirect::to("/account"))
}

fn read<O: Find>(state: &AppState<O>) -> Result<Option<Account>> {
    #[expect(
        clippy::expect_used,
        clippy::unwrap_in_result,
        reason = "ACCOUNT_REF is a fixed, compile-time-known-valid refname literal"
    )]
    let name: gix::refs::FullName = namespace::ACCOUNT_REF
        .try_into()
        .expect("fixed, valid refname");
    let Some(tip) = state.refs.get(name.as_ref())? else {
        return Ok(None);
    };
    let tree = super::commit_tree(&*state.objects(), tip)?;
    Ok(Some(facet_git_tree::deserialize::<Account>(
        &tree,
        &*state.objects(),
    )?))
}

/// Resolve `pubkey` to the enrolled member whose stored key matches it
/// (its id and full [`Member`] record), or [`Error::NotFound`] when none
/// does — shared with `crate::pages::commits::review`, which needs the
/// same "which member is this session" lookup to key a review's composite
/// `refs/meta/reviews/<target>/<member>` ref (`model.review`), and with
/// [`show`]'s own signed-in-as card.
///
/// # Errors
///
/// [`Error::NotFound`] if no enrolled member's key matches `pubkey`;
/// otherwise propagates a ref-store or object read failure.
pub(crate) fn resolve_member_by_key<O: Find>(
    state: &AppState<O>,
    pubkey: &str,
) -> Result<(MemberId, Member)> {
    for entry in state.refs.iter_prefix("refs/meta/member/")? {
        let (name, tip) = entry?;
        let path = name.as_bstr().to_string();
        let Some(username) = path.strip_prefix("refs/meta/member/") else {
            continue;
        };
        let tree = super::commit_tree(&*state.objects(), tip)?;
        if let Ok(member) = facet_git_tree::deserialize::<Member>(&tree, &*state.objects())
            && member.key == pubkey
        {
            return Ok((MemberId::new(username), member));
        }
    }
    Err(Error::NotFound {
        what: "member for the current signing identity".to_owned(),
    })
}
