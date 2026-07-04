//! The generic card every list-shaped meta-ref component renders with: load
//! its items off the async runtime, then a header (title, count badge),
//! an error row, an empty-state message, or one [`Render`]ed row per item.
//!
//! Not every component's page fits this shape — Issues filters to open-only
//! and shows dual open/closed counts in place of a single badge, chrome this
//! card does not have a hook for — so [`super::pages::issues_page`] reuses
//! only [`load`] and each issue's `Render` impl, keeping its own header and
//! body. Forcing that case through [`card`] would mean adding a header
//! override parameter used by exactly one component, which the component
//! plan's own trait-bloat rule rules out.

use std::path::Path;

use git_ents::component::Component;
use maud::{Markup, html};

use super::render::Render;

/// A meta-ref component whose items list and render generically: identity
/// metadata and a [`Render`] impl (both from `git_ents::component`), plus how
/// its items load and what a card shows when there are none yet.
pub(super) trait WebComponent: Component + Render + Send + Sized + 'static {
    /// The card/page title.
    const TITLE: &'static str;
    /// The card body's empty-state message.
    fn empty() -> Markup;
    /// The component's items. Sync — `git_ents::*::load`/`list` shell out to
    /// git and read the object database synchronously — so callers wrap it in
    /// exactly one [`load`].
    fn load(repo: &Path) -> Result<Vec<Self>, String>;
}

/// Load `T`'s items off the async runtime, wrapping [`WebComponent::load`] in
/// the one `spawn_blocking` every component needs.
pub(super) async fn load<T: WebComponent>(repo: &Path) -> Result<Vec<T>, String> {
    let repo = repo.to_owned();
    tokio::task::spawn_blocking(move || T::load(&repo))
        .await
        .map_err(|err| err.to_string())?
}

/// The card chrome every list-shaped component shares: a header with
/// `T::TITLE` and a count badge, then an error row, `T::empty()`, or one
/// rendered row per item.
pub(super) fn card<T: WebComponent>(items: &Result<Vec<T>, String>) -> Markup {
    html! {
        div.card {
            div.card-header {
                (T::TITLE)
                @if let Ok(items) = items { span.count { (items.len()) } }
            }
            @match items {
                Err(err) => div.card-row.muted { "Could not read " (T::PLURAL) ": " (err) }
                Ok(items) if items.is_empty() => (T::empty())
                Ok(items) => { @for item in items { (item.render()) } }
            }
        }
    }
}
