//! WS0 — the interim hydration backend (`docs/scale-out.adoc`, "WS0 —
//! Interim hydration backend"): stock `git http-backend` over ephemeral
//! disk, hydrated from the durable stores (`refstore-postgres` for refs,
//! `odb-tigris` for packs). Not a hack outside the architecture — this
//! crate *is* the stock-git-wrapped backend the protocol traits permit,
//! built first, exactly as the doc's decision record on invariant
//! stratification describes.
//!
//! # Read path
//!
//! [`hydrate::ensure_hydrated`] copies a repository's registered packs
//! (`.pack` + `.idx`, from [`odb_tigris::registry::PackRegistry`]) into
//! `objects/pack/` of a local bare repository, skipping any pack already
//! present by its own content-addressed filename — idempotent, and cheap on
//! every call after the first: ephemeral disk death means nothing more than
//! re-copying everything again next time (`docs/scale-out.adoc`: "nothing
//! correctness-bearing on ephemeral disk"). [`packed_refs::regenerate`]
//! rewrites `packed-refs` from one `RefStore::iter_prefix("refs/")` scan,
//! atomically, to bound advertisement staleness — call it on every
//! `info/refs` request, not just the first.
//!
//! # Write path
//!
//! [`pre_receive::run`] is the `pre-receive` hook body for a repository
//! configured with a [`config::HydrateConfig`]: it authenticates and applies
//! the push through [`git_protocol::native::NativeBackend::receive`]
//! (`IngestPack`) against a [`resolver::PostgresResolver`] — Postgres as the
//! ref store, Tigris (or a local directory in tests) as the object store —
//! exactly the "IngestPack via receive-pack against a scratch repo with
//! Postgres as the commit point" shape `docs/scale-out.adoc`'s "Protocol
//! traits" section names as a conforming implementation. `receive-pack`'s
//! own tmp objdir plays no special role here (unlike a hand-rolled
//! quarantine): staging, the atomic ref transaction, and promotion are all
//! `NativeBackend::receive`'s existing, already-tested ordering, so causal
//! collection safety holds by construction, not by convention. Local disk
//! is a demoted cache: git's own post-hook ref update reconciles it to
//! match Postgres automatically, since our applied edits are the exact ones
//! `receive-pack` was asked to make. The one ref this doesn't reconcile
//! locally — `refs/meta/ops/log`, added to the same atomic transaction
//! internally — self-heals on the next `info/refs` (packed-refs
//! regeneration reads every ref back from Postgres, this one included).
//!
//! Every accepted push through this path also logs a
//! [`git_protocol::CorpusEntry`] (see [`refstore_postgres::PostgresRefStore::log_corpus_entry`]):
//! the seed corpus `backend_conformance::replay_corpus` replays against the
//! local files backends (WS2).
//!
//! # Known limits (short-term, accepted)
//!
//! - Whole-pack hydration makes first-touch read latency scale with repo
//!   size; ranged reads (WS5) are the fix, not this crate's job.
//! - Concurrent pushes to one repo from multiple serve machines are safe
//!   under Postgres's compare-and-swap (no split-brain ref state is ever
//!   possible), but a machine whose local disk cache is stale relative to
//!   another machine's last-accepted push will advertise a stale `old` and
//!   see spurious rejections until its next `info/refs` re-hydration. Pin
//!   writes for one repository to one machine, or accept client retries.

pub mod config;
pub mod hydrate;
pub mod packed_refs;
pub mod pre_receive;
pub mod resolver;

pub use config::HydrateConfig;
