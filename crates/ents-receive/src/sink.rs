//! `EventSink`: the sole destination for post-receive matches
//! (`receive.event-sink`) — null locally, a durable queue hosted.
//!
//! This module also carries the two reference implementations named by the
//! development plan for this phase: [`NullEventSink`] (the null sink the
//! phase-4 exit criterion runs against) and [`MemoryEventSink`] (an
//! in-memory, deduplicating sink demonstrating `receive.dedup` and, paired
//! with [`crate::reconcile`], `receive.reconstructible`).

use std::collections::BTreeSet;
use std::sync::{Mutex, MutexGuard, PoisonError};

use gix_hash::ObjectId;

use crate::error::Result;

/// The sole destination for post-receive matches (`receive.event-sink`):
/// `receive` enqueues one `(effect, oid)` obligation per commit that enters
/// an effect's work set, and never evaluates the effect itself
/// (`receive.never-blocks`).
///
/// # Errors
///
/// [`EventSink::enqueue`] fails only when the sink itself cannot durably
/// record the obligation (queue I/O, hosted). It is never where an effect
/// runs or where a verdict is judged.
///
/// # Examples
///
/// A minimal sink that just counts deliveries — enough to see that
/// `receive` calls `enqueue` at all, without needing the full dedup
/// bookkeeping [`MemoryEventSink`] provides.
///
/// ```
/// use std::sync::atomic::{AtomicUsize, Ordering};
///
/// use ents_receive::EventSink;
///
/// #[derive(Default)]
/// struct Counting(AtomicUsize);
///
/// impl EventSink for Counting {
///     fn enqueue(&self, _effect: &str, _oid: gix_hash::ObjectId) -> ents_receive::Result<()> {
///         self.0.fetch_add(1, Ordering::Relaxed);
///         Ok(())
///     }
/// }
///
/// let sink = Counting::default();
/// sink.enqueue("unit", gix_hash::ObjectId::null(gix_hash::Kind::Sha1))
///     .expect("infallible sink");
/// assert_eq!(sink.0.load(Ordering::Relaxed), 1);
/// ```
// @relation(receive.event-sink, receive.never-blocks, scope=file)
pub trait EventSink: Send + Sync {
    /// Enqueue re-evaluation of `effect` for `oid`.
    ///
    /// Redelivering the same `(effect, oid)` pair MUST be safe to call
    /// again — the dedup key is exactly this pair (`receive.dedup`), so a
    /// conforming sink either folds the duplicate itself ([`MemoryEventSink`]
    /// does) or leaves de-duplication to whatever drains the queue, as long
    /// as the eventual *outcome* is exactly-once.
    ///
    /// # Errors
    ///
    /// Only a genuine sink failure (durable-queue I/O); see the trait's
    /// own doc.
    fn enqueue(&self, effect: &str, oid: ObjectId) -> Result<()>;
}

/// The null `EventSink`: drops every obligation.
///
/// This is the local deployment's reference sink (`receive.event-sink`:
/// "null locally") and the one the phase-4 exit criterion runs `receive`
/// against — a local write path with no effect crate linked yet has nothing
/// useful to enqueue into.
///
/// # Examples
///
/// ```
/// use ents_receive::{EventSink, NullEventSink};
///
/// let sink = NullEventSink;
/// sink.enqueue("unit", gix_hash::ObjectId::null(gix_hash::Kind::Sha1))
///     .expect("the null sink never fails");
/// ```
// @relation(receive.event-sink, scope=file)
#[derive(Debug, Clone, Copy, Default)]
pub struct NullEventSink;

impl EventSink for NullEventSink {
    fn enqueue(&self, _effect: &str, _oid: ObjectId) -> Result<()> {
        Ok(())
    }
}

/// An in-memory, deduplicating `EventSink`: the reference implementation
/// `receive.dedup` and `receive.reconstructible` describe.
///
/// Redelivering the same `(effect, oid)` pair is a no-op — the set, not a
/// counter, is the state — which is what makes redelivery from an
/// at-least-once queue yield exactly-once outcomes (`receive.dedup`). This
/// type MAY lose its state on crash (it is exactly that: in-memory); the
/// composition root is expected to call [`crate::reconcile`] against
/// repository state at startup to rebuild it before serving further pushes,
/// per `receive.reconstructible` — the durable queue this stands in for is a
/// performance optimization, never a correctness requirement.
///
/// # Examples
///
/// ```
/// use ents_receive::{EventSink, MemoryEventSink};
///
/// let sink = MemoryEventSink::default();
/// let oid = gix_hash::ObjectId::null(gix_hash::Kind::Sha1);
///
/// sink.enqueue("unit", oid).expect("infallible sink");
/// sink.enqueue("unit", oid).expect("redelivery is a no-op");
///
/// assert_eq!(sink.pending(), vec![("unit".to_owned(), oid)]);
/// ```
// @relation(receive.dedup, receive.reconstructible, scope=file)
#[derive(Debug, Default)]
pub struct MemoryEventSink {
    pending: Mutex<BTreeSet<(String, ObjectId)>>,
}

impl MemoryEventSink {
    /// Every distinct `(effect, oid)` obligation enqueued so far, in
    /// sorted order.
    #[must_use]
    pub fn pending(&self) -> Vec<(String, ObjectId)> {
        self.locked().iter().cloned().collect()
    }

    fn locked(&self) -> MutexGuard<'_, BTreeSet<(String, ObjectId)>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl EventSink for MemoryEventSink {
    // @relation(receive.dedup, scope=function)
    fn enqueue(&self, effect: &str, oid: ObjectId) -> Result<()> {
        self.locked().insert((effect.to_owned(), oid));
        Ok(())
    }
}
