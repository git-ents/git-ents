//! Integration coverage for `git ents effect list`/`log` output against a
//! real local composition root (`roots.local`): the human listing and the
//! stable `--porcelain` record grammar (`lens.parity`), both derived from
//! [`ents_model::Effect`]'s and [`ents_model::ResultRecord`]'s own
//! `#[facet(ents::...)]`-annotated shapes.

#![allow(clippy::expect_used, reason = "integration test")]

mod common;

use std::process::Command;

use ents_model::Status;
use git_ents::commands::effect;
use git_ents::root::LocalRoot;

fn run(fixture: &common::Fixture, args: &[&str]) -> String {
    let output = Command::new(common::bin_path())
        .current_dir(fixture.path())
        .args(args)
        .output()
        .expect("runs");
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).expect("utf8")
}

/// `git ents effect list --porcelain` emits the stable record grammar
/// (`lens.parity`): a `<name>` head line, then `trigger`, `toolchains`
/// (omitted when empty), and `run` keyed lines, records blank-line
/// separated; the human listing stays `<name>\t<trigger>`.
// @relation(lens.porcelain, lens.parity, model.effect-definition, roots.local, scope=function, role=Verifies)
#[test]
fn effect_list_porcelain_emits_one_record_per_definition() {
    let fixture = common::Fixture::new(1);
    let root = LocalRoot::open(fixture.path()).expect("opens");
    effect::add(
        &root,
        "unit",
        "rev(refs/heads/main)".to_owned(),
        "cargo test".to_owned(),
        vec![],
        Some(fixture.key_path.clone()),
    )
    .expect("defines");
    effect::add(
        &root,
        "with-tools",
        "rev(refs/heads/main)".to_owned(),
        "cargo build".to_owned(),
        vec!["rust-stable".to_owned(), "node-lts".to_owned()],
        Some(fixture.key_path.clone()),
    )
    .expect("defines");

    assert_eq!(
        run(&fixture, &["effect", "list", "--porcelain"]),
        "unit\ntrigger rev(refs/heads/main)\nrun cargo test\n\
         \n\
         with-tools\ntrigger rev(refs/heads/main)\ntoolchains rust-stable, node-lts\nrun cargo build\n"
    );
    assert_eq!(
        run(&fixture, &["effect", "list"]),
        "unit\trev(refs/heads/main)\nwith-tools\trev(refs/heads/main)\n"
    );
}

/// `git ents effect log --porcelain` emits one `<commit> <status>` record
/// per judged commit, the commit's full oid (`model.result-identity`,
/// `lens.parity`); the human listing abbreviates it like git does.
// @relation(lens.porcelain, lens.parity, model.result-identity, roots.local, scope=function, role=Verifies)
#[test]
fn effect_log_porcelain_carries_the_full_judged_commit_oid() {
    let fixture = common::Fixture::new(2);
    let root = LocalRoot::open(fixture.path()).expect("opens");

    let target = gix_hash::ObjectId::from_hex(b"0123456789abcdef0123456789abcdef01234567")
        .expect("valid hex");
    let signer = git_ents::sign::Signer::load(&fixture.key_path).expect("loads");
    let author = gix::actor::Signature {
        name: "worker".into(),
        email: "worker@ents.test".into(),
        time: gix::date::Time {
            seconds: 1_000,
            offset: 0,
        },
    };
    let results_ref =
        ents_model::namespace::result_ref("unit", &ents_effect::run::short_oid(target))
            .expect("valid refname");
    ents_effect::write_result(
        &root.refs,
        &root.objects,
        &root.events,
        results_ref,
        "unit",
        target,
        Status::Pass,
        &author,
        |payload| signer.sign(payload),
        ents_receive::Mode::Advisory,
    )
    .expect("records");

    assert_eq!(
        run(&fixture, &["effect", "log", "unit", "--porcelain"]),
        "0123456789abcdef0123456789abcdef01234567 pass\n"
    );
    assert_eq!(run(&fixture, &["effect", "log", "unit"]), "0123456\tpass\n");
}
