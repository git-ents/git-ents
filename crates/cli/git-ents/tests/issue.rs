//! Integration coverage for `git ents issue` against a real local
//! composition root (`roots.local`): `new` via an explicit `--title`
//! (a direct library call), `new` via a fake `$EDITOR` script run through
//! the actual `git-ents` binary (the interactive-composition path — a
//! subprocess so `$EDITOR` is scoped to the child rather than mutating
//! this test process's own environment), and `edit` round-tripping
//! state/assignees/labels.

#![allow(clippy::expect_used, reason = "integration test")]

mod common;

use std::process::Command;

use ents_model::MemberId;
use git_ents::commands::issue;
use git_ents::root::LocalRoot;

/// Write an executable script at `path` that overwrites its one argument
/// (the scratch file `git ents issue new` opens) with `contents` — a
/// stand-in for a real `$EDITOR`, exercising the same
/// spawn-and-read-back path a real editor would.
fn write_fake_editor(path: &std::path::Path, contents: &str) {
    let script = format!("#!/bin/sh\ncat > \"$1\" <<'EOF'\n{contents}\nEOF\n");
    std::fs::write(path, script).expect("write fake editor");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mut perms = std::fs::metadata(path).expect("metadata").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).expect("chmod");
    }
}

/// `git ents issue new --title ...`: no editor needed, the title and body
/// round-trip exactly.
// @relation(model.issue, roots.local, scope=function, role=Verifies)
#[test]
fn issue_new_with_an_explicit_title_skips_the_editor() {
    let fixture = common::Fixture::new(1);
    let root = LocalRoot::open(fixture.path()).expect("opens");

    let id = issue::new(
        &root,
        Some("gate rejects a valid signature".to_owned()),
        Some("steps to reproduce...".to_owned()),
        "open".to_owned(),
        vec!["bug".to_owned()],
        vec!["jdc".to_owned()],
        Some(fixture.key_path.clone()),
    )
    .expect("creates");

    let found = issue::show(&root, &id).expect("shows");
    assert_eq!(found.title, "gate rejects a valid signature");
    assert_eq!(found.body, "steps to reproduce...");
    assert_eq!(found.state, "open");
    assert_eq!(found.labels, vec!["bug".to_owned()]);
    assert_eq!(found.assignees, vec![MemberId::new("jdc")]);
}

/// `git ents issue new` with no `--title`, run as the real binary with a
/// fake `$EDITOR`: composes the title and body from the scratch file —
/// first line title, remaining lines body, `#` lines stripped.
// @relation(model.issue, roots.local, scope=function, role=Verifies)
#[test]
fn issue_new_composes_title_and_body_from_a_fake_editor() {
    let fixture = common::Fixture::new(2);
    let editor_path = fixture.path().join("fake-editor.sh");
    write_fake_editor(
        &editor_path,
        "issue title from the editor\nfirst body line\nsecond body line\n# a stray comment line",
    );

    let output = Command::new(common::bin_path())
        .current_dir(fixture.path())
        .args(["issue", "new", "--state", "open", "--key"])
        .arg(&fixture.key_path)
        .env("GIT_EDITOR", &editor_path)
        .env("EDITOR", &editor_path)
        .output()
        .expect("runs");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).expect("utf8");
    let id = stdout
        .trim()
        .strip_prefix("opened ")
        .expect("prints \"opened <id>\"")
        .to_owned();

    let root = LocalRoot::open(fixture.path()).expect("opens");
    let found = issue::show(&root, &id).expect("shows");
    assert_eq!(found.title, "issue title from the editor");
    assert_eq!(found.body, "first body line\nsecond body line");
}

/// An empty editor message (blank title after stripping `#` lines) aborts
/// the command with a failing exit status, mirroring `git commit`'s own
/// empty-message abort.
// @relation(model.issue, roots.local, scope=function, role=Verifies)
#[test]
fn issue_new_aborts_on_an_empty_editor_message() {
    let fixture = common::Fixture::new(3);
    let editor_path = fixture.path().join("fake-editor.sh");
    write_fake_editor(&editor_path, "# only a comment, no title");

    let output = Command::new(common::bin_path())
        .current_dir(fixture.path())
        .args(["issue", "new", "--state", "open", "--key"])
        .arg(&fixture.key_path)
        .env("GIT_EDITOR", &editor_path)
        .env("EDITOR", &editor_path)
        .output()
        .expect("runs");
    assert!(
        !output.status.success(),
        "an empty title must abort issue creation: {output:?}"
    );
}

/// `git ents issue edit`: state, assignees, and labels round-trip through
/// an edit on top of the issue's existing tip.
// @relation(model.issue, roots.local, scope=function, role=Verifies)
#[test]
fn issue_edit_round_trips_state_assignees_and_labels() {
    let fixture = common::Fixture::new(4);
    let root = LocalRoot::open(fixture.path()).expect("opens");

    let id = issue::new(
        &root,
        Some("title".to_owned()),
        Some("body".to_owned()),
        "open".to_owned(),
        vec![],
        vec![],
        Some(fixture.key_path.clone()),
    )
    .expect("creates");

    issue::edit(
        &root,
        &id,
        Some("triaged".to_owned()),
        vec!["bug".to_owned(), "gate".to_owned()],
        vec!["jdc".to_owned(), "ci-worker".to_owned()],
        Some(fixture.key_path.clone()),
    )
    .expect("edits");

    let found = issue::show(&root, &id).expect("shows");
    assert_eq!(found.state, "triaged");
    assert_eq!(found.labels, vec!["bug".to_owned(), "gate".to_owned()]);
    assert_eq!(
        found.assignees,
        vec![MemberId::new("jdc"), MemberId::new("ci-worker")]
    );
    // Title and body are untouched by an edit that names neither.
    assert_eq!(found.title, "title");
    assert_eq!(found.body, "body");
}
