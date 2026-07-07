//! Property functions for [`git_backend::ObjectStore`] implementations —
//! the same suite run against every backend (`docs/scale-out.adoc`,
//! "Storage traits" / WS2).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "assertion helpers for a conformance suite, not application code"
)]

use git_backend::{ObjectStore, PackStream};

use crate::collector::Collector;
use crate::support::oid_and_pack;

/// Run every [`ObjectStore`] property against a fresh backend built by
/// `mk`, using `collector` to drive the causal-collection-safety property.
/// Each property gets its own fresh backend instance (a fresh call to
/// `mk`) so one property's writes never leak into another's assertions.
pub fn object_store_properties<S, C>(mk: impl Fn() -> S, collector: &C)
where
    S: ObjectStore,
    C: Collector,
{
    quarantine_invisibility(&mk());
    causal_collection_safety(&mk(), collector);
}

/// Staged objects are invisible to `read`/`contains` until promoted; once
/// promoted, they're visible and correct (`docs/scale-out.adoc`,
/// "ObjectStore").
pub fn quarantine_invisibility<S: ObjectStore>(store: &S) {
    let fixture = oid_and_pack();

    assert!(
        !store
            .contains(fixture.oid)
            .expect("contains before staging"),
        "a fresh store must not already contain the fixture object"
    );

    let quarantine = store
        .stage_pack(PackStream::new(std::io::Cursor::new(fixture.pack.clone())))
        .expect("stage_pack");
    assert!(
        !store.contains(fixture.oid).expect("contains while staged"),
        "a staged, unpromoted object must be invisible to contains"
    );
    assert!(
        store.read(fixture.oid).is_err(),
        "a staged, unpromoted object must be invisible to read"
    );

    store.promote(quarantine).expect("promote");
    assert!(
        store.contains(fixture.oid).expect("contains after promote"),
        "a promoted object must be visible to contains"
    );
    let object = store.read(fixture.oid).expect("read after promote");
    assert_eq!(object.kind, gix_object::Kind::Commit);
}

/// Objects staged for an in-flight transaction are never collected
/// (`docs/scale-out.adoc`, correctness rule 1). `collector` stands in for
/// whatever collection mechanism a backend has; today's local backends
/// have none wired up ([`crate::NoopCollector`]), which still exercises
/// the quarantine path a real collector must also respect, without
/// asserting a collection arm no backend implements yet. A backend with a
/// time-bounded staging grace window (`Collector::staging_grace`) is
/// responsible for asserting its own boundary — that a session which can't
/// finish inside the window aborts rather than becoming collectible
/// mid-flight — in its own instantiation, since only it knows how to hold
/// a staging session open past its deadline.
pub fn causal_collection_safety<S: ObjectStore, C: Collector>(store: &S, collector: &C) {
    let fixture = oid_and_pack();
    let quarantine = store
        .stage_pack(PackStream::new(std::io::Cursor::new(fixture.pack.clone())))
        .expect("stage_pack");

    // A collection pass runs while the object is still only staged — the
    // in-flight transaction hasn't committed (promoted) yet.
    collector.collect();

    // Regardless of what the collector did, the staged object must still
    // be promotable and, once promoted, readable and correct: a collector
    // that reaped a staged object would make one of these fail.
    store
        .promote(quarantine)
        .expect("promote after a collection pass");
    assert!(
        store.contains(fixture.oid).expect("contains after promote"),
        "a collection pass during staging must not have reaped the staged object"
    );
    let object = store.read(fixture.oid).expect("read after promote");
    assert_eq!(object.kind, gix_object::Kind::Commit);
}
