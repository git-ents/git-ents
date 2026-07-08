//! Re-exports `gix-reachability`'s shared reachability walk (WS6 moved the
//! implementation there — see that crate's `walk` module docs for why:
//! it's where the commit-graph accelerator this walk degrades from now
//! lives). Kept as a `walk` module here too so `negotiate`/`ingest`'s
//! existing `crate::walk::{...}` imports, and this crate's own tests,
//! didn't need to change along with the move.
//!
//! [`crate::native::negotiate`] and [`crate::native::ingest`] call
//! [`gix_reachability::engine::accelerated_reachable`] directly rather than
//! [`reachable`] — the accelerated entry point wraps this walk, it isn't
//! re-exported under this name too, so the call sites make plain which one
//! they're using.

pub use gix_reachability::walk::{ObjectSource, StoreSource, reachable};
