//! Integration coverage for `git ents config` against a real local
//! composition root (`roots.local`): narrowing the agent-runtime defaults
//! at `refs/meta/config` without disturbing fields `set` was not told to
//! touch, and reading an unconfigured repository back as
//! `ents_gate::Config::default()`.

#![allow(clippy::expect_used, reason = "integration test")]

mod common;

use ents_gate::Config;
use git_ents::commands::config;
use git_ents::root::LocalRoot;

/// A repository that has never had `refs/meta/config` written reads back
/// as every field unset -- the same "absent means unconfigured" reading
/// `ents_gate::config`'s own readers give, and what lets an old config
/// (predating `agent_provider`/`agent_default_model`) keep parsing.
// @relation(roots.local, scope=function, role=Verifies)
#[test]
fn absent_config_reads_as_default() {
    let fixture = common::Fixture::new(1);
    let root = LocalRoot::open(fixture.path()).expect("opens");

    let config = config::show(&root).expect("shows");
    assert_eq!(config, Config::default());
}

/// `set` narrows one field at a time: a later call naming only
/// `agent_default_model` must not clobber an `agent_provider` an earlier
/// call set, and neither call ever touches `workers`/`epoch` (unset by
/// either call).
// @relation(roots.local, scope=function, role=Verifies)
#[test]
fn set_narrows_without_clobbering_other_fields() {
    let fixture = common::Fixture::new(1);
    let root = LocalRoot::open(fixture.path()).expect("opens");

    config::set(
        &root,
        Some("anthropic".to_owned()),
        None,
        Some(fixture.key_path.clone()),
    )
    .expect("sets provider");
    let config = config::show(&root).expect("shows");
    assert_eq!(config.agent_provider.as_deref(), Some("anthropic"));
    assert_eq!(config.agent_default_model, None);
    assert!(config.workers.is_empty());
    assert_eq!(config.epoch, None);

    config::set(
        &root,
        None,
        Some("claude-sonnet-5".to_owned()),
        Some(fixture.key_path.clone()),
    )
    .expect("sets default model");
    let config = config::show(&root).expect("shows");
    assert_eq!(config.agent_provider.as_deref(), Some("anthropic"));
    assert_eq!(
        config.agent_default_model.as_deref(),
        Some("claude-sonnet-5")
    );
}
