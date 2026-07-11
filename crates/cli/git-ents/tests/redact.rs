//! Integration coverage for `git ents redact` against a real local
//! composition root (`roots.local`) — recording a redaction, then listing
//! it back (`model.redaction`).

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "integration test"
)]

mod common;

use git_ents::commands::redact;
use git_ents::root::LocalRoot;

/// `git ents redact list` surfaces every recorded redaction's id and
/// reason — the only way to discover one before `git ents redact add`'s
/// own record can be inspected again (`model.redaction`).
// @relation(roots.local, model.redaction, scope=function, role=Verifies)
#[test]
fn list_returns_every_recorded_redaction() {
    let fixture = common::Fixture::new(1);
    let root = LocalRoot::open(fixture.path()).expect("opens");

    let oid = "abababababababababababababababababababab";
    redact::add(
        &root,
        oid,
        "leaked credential".to_owned(),
        Some(fixture.key_path.clone()),
    )
    .expect("adds");

    let listed = redact::list(&root).expect("lists");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].0, oid);
    assert_eq!(listed[0].1.reason, "leaked credential");
}
