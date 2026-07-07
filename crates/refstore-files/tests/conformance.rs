//! This crate's instantiation of the shared backend conformance suite
//! (`docs/scale-out.adoc`, WS2): every `RefStore` property run against
//! `FilesRefStore`.

use backend_conformance::WithScratchRepo;
use refstore_files::FilesRefStore;

#[test]
fn conforms_to_ref_store_properties() {
    backend_conformance::ref_store_properties(|| WithScratchRepo::new(FilesRefStore::open));
}
