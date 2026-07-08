//! Real [`backend_conformance::Collector`]s — the seam WS2 left open, now
//! closed: the causal-collection-safety property (`docs/scale-out.adoc`,
//! correctness rule 1) runs against collection passes that actually
//! collect, instead of [`backend_conformance::NoopCollector`].
//!
//! Both collectors panic if a collection pass errors: they exist to drive
//! a conformance property, and a collector that swallowed its own failure
//! would let the property pass vacuously ("no fake assertions").

use std::time::Duration;

use backend_conformance::Collector;
use git_backend::RefStore;
use odb_tigris::OdbTigris;
use odb_tigris::registry::PackRegistry;
use odb_tigris::transport::BlobTransport;

/// A [`Collector`] over one [`OdbTigris`] store: one collection pass =
/// expire staging sessions past their grace window (the grace-based cruft
/// arm of rule 1, a no-op for a store without one), then a full
/// mark-and-sweep ([`crate::gc::collect`]).
pub struct TigrisCollector<'a, T, R> {
    repo_id: &'a str,
    refs: &'a dyn RefStore,
    store: &'a OdbTigris<T, R>,
    transport: &'a dyn BlobTransport,
    registry: &'a dyn PackRegistry,
}

impl<'a, T, R> TigrisCollector<'a, T, R>
where
    T: BlobTransport,
    R: PackRegistry,
{
    /// A collector over `store`, marking from `refs` and sweeping via
    /// `registry`/`transport` — the same transport and registry `store`
    /// itself was built over.
    #[must_use]
    pub fn new(
        repo_id: &'a str,
        refs: &'a dyn RefStore,
        store: &'a OdbTigris<T, R>,
        transport: &'a dyn BlobTransport,
        registry: &'a dyn PackRegistry,
    ) -> Self {
        Self {
            repo_id,
            refs,
            store,
            transport,
            registry,
        }
    }
}

impl<T, R> Collector for TigrisCollector<'_, T, R>
where
    T: BlobTransport,
    R: PackRegistry,
{
    #[expect(
        clippy::expect_used,
        reason = "conformance driver: a failed collection pass must fail the \
                  property loudly, never let it pass vacuously"
    )]
    fn collect(&self) {
        self.store
            .expire_stale_quarantines()
            .expect("expire stale quarantines");
        crate::gc::collect(
            self.repo_id,
            self.refs,
            self.store,
            self.transport,
            self.registry,
        )
        .expect("mark-and-sweep collection pass");
    }

    fn staging_grace(&self) -> Option<Duration> {
        self.store.staging_grace()
    }
}

/// A [`Collector`] over a local bare repository (`refstore-files` +
/// `odb-files`): one collection pass = [`crate::gc::collect_files`]. No
/// grace window ([`Collector::staging_grace`] stays `None`) — the local
/// backend bounds staging by promotion alone, never by a clock.
pub struct FilesCollector {
    repo: std::path::PathBuf,
}

impl FilesCollector {
    /// A collector over the bare repository at `repo`.
    #[must_use]
    pub fn new(repo: impl Into<std::path::PathBuf>) -> Self {
        Self { repo: repo.into() }
    }
}

impl Collector for FilesCollector {
    #[expect(
        clippy::expect_used,
        reason = "conformance driver: a failed collection pass must fail the \
                  property loudly, never let it pass vacuously"
    )]
    fn collect(&self) {
        crate::gc::collect_files(&self.repo).expect("files mark-and-sweep collection pass");
    }
}
