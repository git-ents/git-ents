//! Building a pack from a fixed list of whole objects — used both by
//! [`crate::native`]'s `GeneratePack` (objects read back from the promoted
//! store) and by the op record's own tiny self-pack (`docs/scale-out.adoc`,
//! "Attested push").
//!
//! Every entry is written as a full base object, never a delta: correct and
//! simple, not space-efficient. `docs/scale-out.adoc"`'s Q6 is exactly this
//! trade-off at scale; WS5/WS6 is where delta reuse and ranged reads belong.

use gix_hash::ObjectId;
use gix_object::Kind;
use gix_pack::data::output::{Count, Entry, bytes::FromEntriesIter};

use crate::{Error, Result};

/// One object to include in a pack built by [`build_pack`].
pub struct PackObject {
    /// The object's id.
    pub id: ObjectId,
    /// The object's kind.
    pub kind: Kind,
    /// The object's raw, undeltified content.
    pub data: Vec<u8>,
}

/// Encode `objects` as a version-2 pack, each as a full base object.
pub fn build_pack(objects: &[PackObject]) -> Result<Vec<u8>> {
    let entries: Vec<Entry> = objects
        .iter()
        .map(|object| {
            let count = Count::from_data(object.id, None);
            let data = gix_object::Data::new(&object.data, object.kind, gix_hash::Kind::Sha1);
            Entry::from_data(&count, &data).map_err(|error| Error::Pack(error.to_string()))
        })
        .collect::<Result<_>>()?;
    let num_entries = u32::try_from(entries.len()).map_err(|_too_many| {
        Error::Pack("cannot encode more than u32::MAX objects in one pack".to_owned())
    })?;
    let input = std::iter::once(Ok::<_, std::convert::Infallible>(entries));
    let mut writer = FromEntriesIter::new(
        input,
        Vec::new(),
        num_entries,
        gix_pack::data::Version::V2,
        gix_hash::Kind::Sha1,
    );
    for step in &mut writer {
        step.map_err(|error| Error::Pack(error.to_string()))?;
    }
    Ok(writer.into_write())
}
