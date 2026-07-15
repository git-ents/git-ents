//! Integration coverage for `git ents toolchain` against a real local
//! composition root (`roots.local`) — importing a toolchain, then listing
//! it back.

#![allow(clippy::expect_used, reason = "integration test")]

mod common;

use git_ents::commands::toolchain;
use git_ents::root::LocalRoot;

/// `git ents toolchain list` names every imported toolchain — the only way
/// to discover a toolchain's name before `view`/`log` can be run against it
/// (`model.toolchain`).
// @relation(roots.local, model.toolchain, scope=function, role=Verifies)
#[test]
fn list_names_every_imported_toolchain() {
    let fixture = common::Fixture::new(1);
    let root = LocalRoot::open(fixture.path()).expect("opens");

    let bin = fixture.path().join("bin");
    std::fs::create_dir(&bin).expect("mkdir");
    std::fs::write(bin.join("tool"), b"#!/bin/sh\necho hi\n").expect("write");

    toolchain::import(&root, "rust-stable", &bin, Some(fixture.key_path.clone())).expect("imports");

    let listed = toolchain::list(&root).expect("lists");
    assert_eq!(listed, vec!["rust-stable".to_owned()]);
}
