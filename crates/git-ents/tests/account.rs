//! Integration coverage for `git ents account` against a real local
//! composition root (`roots.local`) — creating this repository's account
//! identity, then reading it back (`model.account`).

#![allow(clippy::expect_used, reason = "integration test")]

mod common;

use git_ents::commands::{account, members};
use git_ents::root::LocalRoot;

/// `git ents account show` reads back exactly what `create` wrote — the
/// only read command against the fixed `refs/meta/account` ref
/// (`model.account`).
// @relation(roots.local, model.account, scope=function, role=Verifies)
#[test]
fn show_reads_back_the_created_identity() {
    let fixture = common::Fixture::new(1);
    let root = LocalRoot::open(fixture.path()).expect("opens");
    members::add(&root, "jdc", None, Some(fixture.key_path.clone())).expect("bootstrap");

    account::create(
        &root,
        Some("jdc".to_owned()),
        "joseph.carpinelli@icloud.com".to_owned(),
        Some(fixture.key_path.clone()),
    )
    .expect("creates");

    let account = account::show(&root).expect("shows");
    assert_eq!(account.member, ents_model::MemberId::new("jdc"));
    assert_eq!(account.login, "joseph.carpinelli@icloud.com");
}
