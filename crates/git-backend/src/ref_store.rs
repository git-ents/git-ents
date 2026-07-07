//! [`RefStore`]: the unit of correctness for repository state.

use gix_hash::ObjectId;

use crate::Result;

/// A full ref name (`refs/heads/main`) or a ref-namespace prefix
/// (`refs/meta/`), used with [`RefStore::iter_prefix`] and
/// [`RefStore::watch`]. Backend-agnostic: it carries no assumption about
/// whether the underlying store is gitoxide loose refs, a Postgres row, or
/// anything else.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RefName(String);

impl RefName {
    /// Build a `RefName` from any owned-or-borrowed string.
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// The ref name as a `&str`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for RefName {
    fn from(name: &str) -> Self {
        Self::new(name)
    }
}

impl From<String> for RefName {
    fn from(name: String) -> Self {
        Self::new(name)
    }
}

impl AsRef<str> for RefName {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl std::fmt::Display for RefName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The compare-and-swap precondition a [`RefEdit`] requires of a ref's
/// current value before the edit is allowed to apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expected {
    /// No requirement: set unconditionally.
    Any,
    /// The ref must not currently exist.
    MustNotExist,
    /// The ref must currently exist and equal the given [`ObjectId`].
    MustExistAndMatch(ObjectId),
}

/// One ref's half of a [`RefStore::transaction`] batch: what `name` is
/// expected to hold, and what it should become. `new: None` deletes the
/// ref.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefEdit {
    /// The ref this edit applies to.
    pub name: RefName,
    /// The compare-and-swap precondition checked against `name`'s current
    /// value before the edit applies.
    pub expected: Expected,
    /// The value to set `name` to, or `None` to delete it.
    pub new: Option<ObjectId>,
}

/// The result of a [`RefStore::transaction`] call that itself completed
/// (returned `Ok`): either every edit applied, or none did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxOutcome {
    /// Every edit in the batch applied atomically.
    Applied,
    /// The transaction did not apply: `name`'s current value did not match
    /// its edit's [`Expected`] precondition. No edit in the batch took
    /// effect — compare-and-swap is all-or-nothing, per the trait's
    /// contract.
    Rejected {
        /// The first ref whose precondition failed.
        name: RefName,
    },
}

/// An iterator over `(name, tip)` pairs from a [`RefStore::iter_prefix`]
/// query, wrapping whatever iterator the backend produces so the trait
/// itself stays object-safe.
pub struct RefIter(Box<dyn Iterator<Item = Result<(RefName, ObjectId)>> + Send>);

impl RefIter {
    /// Wrap `iter` as a [`RefIter`].
    pub fn new(iter: impl Iterator<Item = Result<(RefName, ObjectId)>> + Send + 'static) -> Self {
        Self(Box::new(iter))
    }
}

impl Iterator for RefIter {
    type Item = Result<(RefName, ObjectId)>;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next()
    }
}

/// One entry in a ref's log: the value it moved from and to, the message
/// recorded with the change, and when it happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefLogEntry {
    /// The ref's value before this entry, or `None` when the ref was
    /// created by it.
    pub old: Option<ObjectId>,
    /// The ref's value after this entry, or `None` when the ref was
    /// deleted by it.
    pub new: Option<ObjectId>,
    /// The message recorded with the change.
    pub message: String,
    /// When the change happened, in seconds since the epoch.
    pub seconds: u64,
}

/// An iterator over a ref's [`RefLogEntry`] history, most recent first.
pub struct RefLogIter(Box<dyn Iterator<Item = Result<RefLogEntry>> + Send>);

impl RefLogIter {
    /// Wrap `iter` as a [`RefLogIter`].
    pub fn new(iter: impl Iterator<Item = Result<RefLogEntry>> + Send + 'static) -> Self {
        Self(Box::new(iter))
    }
}

impl Iterator for RefLogIter {
    type Item = Result<RefLogEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next()
    }
}

/// A wakeup hint delivered by a [`RefEventStream`]. Carries no payload: per
/// [`RefStore::watch`]'s contract, a consumer never trusts the event's
/// content, only that *something* changed under the watched prefix, and
/// re-drains its own source of truth (a queue table, a fresh
/// [`RefStore::iter_prefix`]) in response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefEvent;

/// A best-effort stream of [`RefEvent`] wakeup hints from
/// [`RefStore::watch`]. Delivery is not guaranteed: a hint can be delayed,
/// coalesced, or dropped entirely (e.g. across a reconnect). Every consumer
/// must therefore drain its own durable state on every wakeup *and* on
/// reconnect, never relying on this stream to have delivered exactly one
/// event per change.
pub struct RefEventStream {
    receiver: std::sync::mpsc::Receiver<RefEvent>,
}

impl RefEventStream {
    /// Wrap `receiver` as a [`RefEventStream`].
    #[must_use]
    pub fn new(receiver: std::sync::mpsc::Receiver<RefEvent>) -> Self {
        Self { receiver }
    }

    /// Block until the next wakeup hint, or `None` once the backend's
    /// watcher has shut down.
    pub fn recv(&self) -> Option<RefEvent> {
        self.receiver.recv().ok()
    }

    /// Block for up to `timeout` for the next wakeup hint.
    pub fn recv_timeout(&self, timeout: std::time::Duration) -> Option<RefEvent> {
        self.receiver.recv_timeout(timeout).ok()
    }
}

/// The unit of correctness for repository state: a store of named refs,
/// each pointing at an [`ObjectId`], updated only through atomic
/// transactions.
///
/// # Contract
///
/// - **Multi-ref compare-and-swap is contractual, not a capability query.**
///   A backend that cannot apply an arbitrary batch of [`RefEdit`]s
///   atomically — every precondition checked against one consistent view,
///   and either every edit applies or none do — does not satisfy this
///   trait, full stop.
/// - **`watch` is a best-effort wakeup hint, never a source of truth.** The
///   effect queue table (or equivalent durable state) is what carries the
///   at-least-once guarantee; a consumer must drain it on every wakeup and
///   on reconnect, not trust that one hint means exactly one change.
/// - **`log` is the ref's own history**, independent of the store's queue —
///   an audit trail, not a delivery mechanism.
pub trait RefStore: Send + Sync {
    /// The object id `name` currently points at, or `None` if `name` does
    /// not exist.
    fn get(&self, name: &RefName) -> Result<Option<ObjectId>>;

    /// Every ref under `prefix`, with its current tip.
    fn iter_prefix(&self, prefix: &RefName) -> Result<RefIter>;

    /// Apply `edits` as one atomic compare-and-swap transaction: every
    /// edit's [`Expected`] precondition is checked against the same
    /// consistent view of the store, and either every edit applies or none
    /// do. See the trait's contract above — this is not optional behavior a
    /// backend may approximate.
    fn transaction(&self, edits: &[RefEdit]) -> Result<TxOutcome>;

    /// Subscribe to a best-effort wakeup hint whenever a ref under `prefix`
    /// changes. See the trait's contract above: delivery is not
    /// guaranteed, and no consumer may treat this stream as a source of
    /// truth.
    fn watch(&self, prefix: &RefName) -> Result<RefEventStream>;

    /// `name`'s history, most recent entry first.
    fn log(&self, name: &RefName) -> Result<RefLogIter>;
}
