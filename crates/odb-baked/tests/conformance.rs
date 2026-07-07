//! This crate's instantiation of the shared backend conformance suite
//! (`docs/scale-out.adoc`, WS2): every `ObjectStore` property run against
//! `BakedTier` composed over `OdbFiles`, with an empty (unbaked) baked
//! directory — a `BakedTier` with nothing baked in must behave exactly like
//! its underlying store, since `stage_pack`/`promote`/`read`/`contains` all
//! pass straight through when [`odb_baked::BakedTier`] has no manifest.

use backend_conformance::{NoopCollector, WithScratchRepo};
use odb_baked::BakedTier;
use odb_files::OdbFiles;

#[test]
fn conforms_to_object_store_properties() {
    backend_conformance::object_store_properties(
        || {
            WithScratchRepo::new(|path| {
                let baked_dir = path.join("baked");
                let underlying = OdbFiles::open(path)?;
                BakedTier::open(&baked_dir, underlying)
            })
        },
        &NoopCollector,
    );
}
