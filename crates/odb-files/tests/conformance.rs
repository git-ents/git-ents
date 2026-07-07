//! This crate's instantiation of the shared backend conformance suite
//! (`docs/scale-out.adoc`, WS2): every `ObjectStore` property run against
//! `OdbFiles`, with no GC wired up yet
//! ([`backend_conformance::NoopCollector`]).

use backend_conformance::{NoopCollector, WithScratchRepo};
use odb_files::OdbFiles;

#[test]
fn conforms_to_object_store_properties() {
    backend_conformance::object_store_properties(
        || WithScratchRepo::new(OdbFiles::open),
        &NoopCollector,
    );
}
