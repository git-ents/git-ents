//! The storage-layout traits every meta-ref component implements, plus the
//! identity metadata the CLI and server share.
//!
//! A component stores itself one of three ways — [`Document`] (a single
//! document on one ref), [`MapDocument`] (named entries in one scalar-keyed
//! map on one ref), or [`Collection`] (one ref per item under a namespace) —
//! and the free functions here are the single place that turns each trait
//! into the matching [`crate::Store`] call, so a module's own
//! `load`/`store` shrinks to a one-line delegation instead of hand-formatting
//! a ref name.

use facet::Facet;

/// A type stored whole on a single meta ref (e.g. a repository's
/// configuration or account profile).
pub trait Document: for<'a> Facet<'a> {
    /// The ref the document lives on.
    const REF: &'static str;
}

/// Load the document at [`Document::REF`], or `None` when the ref is absent.
pub fn load<T: Document>(store: &crate::Store) -> Result<Option<T>, crate::Error> {
    store.load(T::REF)
}

/// Write `value` to [`Document::REF`], replacing any existing value as a new
/// commit.
pub fn store<T: Document>(
    store: &crate::Store,
    value: &T,
    message: &str,
) -> Result<(), crate::Error> {
    store.store(T::REF, value, message)
}

/// A type stored as one `<key> -> body` map document on a single ref (e.g. a
/// configured check set or a revocation list).
pub trait MapDocument: Sized {
    /// The ref the map document lives on.
    const REF: &'static str;
    /// The value type stored per map key.
    type Body: for<'a> Facet<'a>;
    /// Assemble the public item from its map key and stored body.
    fn compose(key: String, body: Self::Body) -> Self;
    /// Split the item back into its map key and stored body.
    fn decompose(&self) -> (&str, Self::Body);
}

/// Load [`MapDocument::REF`]'s entries as their flattened item list. An
/// absent ref yields an empty list.
pub fn load_map<T: MapDocument>(store: &crate::Store) -> Result<Vec<T>, crate::Error> {
    store.load_map(T::REF, T::compose)
}

/// Replace [`MapDocument::REF`]'s entries with `items`.
pub fn store_map<T: MapDocument>(
    store: &crate::Store,
    items: &[T],
    message: &str,
) -> Result<(), crate::Error> {
    store.store_map(
        T::REF,
        items,
        |item| {
            let (key, body) = item.decompose();
            (key.to_owned(), body)
        },
        message,
    )
}

/// A type stored decomposed, one ref per item, under a namespace (e.g. a
/// member or an issue). Deliberately not bound on [`crate::HasId`]: an
/// issue's ref key is its genesis hash, a value never stored inside the
/// document itself, so [`load_item`]/[`store_item`] take the id explicitly;
/// [`store_keyed`] is the add-on for a collection (like a member) whose item
/// legitimately carries its own key.
pub trait Collection: for<'a> Facet<'a> {
    /// The ref namespace (`{NS}/{id}` per item) its items live under.
    const NS: &'static str;
}

/// Load the item `id` under [`Collection::NS`], or `None` when its ref is
/// absent.
pub fn load_item<T: Collection>(store: &crate::Store, id: &str) -> Result<Option<T>, crate::Error> {
    store.load_item(T::NS, id)
}

/// Store `value` as item `id` under [`Collection::NS`].
pub fn store_item<T: Collection>(
    store: &crate::Store,
    id: &str,
    value: &T,
    message: &str,
) -> Result<(), crate::Error> {
    store.store_item(T::NS, id, value, message)
}

/// Store `value` as item [`crate::HasId::id`] under [`Collection::NS`],
/// for a collection whose item carries its own key.
pub fn store_keyed<T: Collection + crate::HasId>(
    store: &crate::Store,
    value: &T,
    message: &str,
) -> Result<(), crate::Error> {
    store.store_keyed(T::NS, value, message)
}

/// Every item under [`Collection::NS`], paired with the id its ref was stored
/// under, newest first.
pub fn list<T: Collection>(store: &crate::Store) -> Result<Vec<(String, T)>, crate::Error> {
    store.list_items(T::NS)
}

/// Identity metadata a component carries for messages and UI chrome, shared
/// by the CLI and the server.
pub trait Component {
    /// The singular noun used in messages ("member", "check", "issue").
    const NOUN: &'static str;
    /// The plural noun ("members", "checks", "issues").
    const PLURAL: &'static str;
}
