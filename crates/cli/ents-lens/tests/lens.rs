//! Integration coverage for `docs/spec/lens.adoc`, driving the [`Lens`]
//! request handlers directly against a fixture repository — the strategy
//! the engineering conventions select for a protocol surface: construct the
//! server in-process with a real working tree and a comment anchored into
//! it, then assert each handler's derived LSP value, rather than spawning a
//! stdio process and parsing frames. The JSON-RPC framing is `lsp-server`'s
//! own tested concern; what this crate owns is the derivation, so that is
//! what these tests exercise.
//!
//! The seams are `ents-testutil`'s in-memory `MemRefStore`/`ObjectStore`
//! (the same pair every library crate's tests use) paired with a real
//! on-disk repository for the working tree the anchors project onto —
//! `ents_forge::comment::add` embeds the anchored bytes into the object
//! store, so the two stay consistent even though only one is on disk.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "integration test"
)]

use std::path::Path;
use std::process::Command;

use ents_forge::comment::{self, NewComment};
use ents_lens::{CMD_COMPOSE, CMD_RESOLVE, CMD_VIEW, Lens, Signing};
use ents_receive::{Identity, Mode, NullEventSink};
use ents_testutil::{Keypair, MemRefStore, ObjectStore};
use lsp_types::{DiagnosticSeverity, HoverContents, Position, Range, Url};
use serde_json::json;

/// A fixture repository, its in-memory seams, and a deterministic signing
/// key — everything a [`Lens`] needs to be wired the way `git ents lsp`
/// wires it.
struct Fixture {
    dir: tempfile::TempDir,
    refs: MemRefStore,
    objects: ObjectStore,
    key: Keypair,
}

impl Fixture {
    /// A repository holding `file.txt` with ten numbered lines, committed.
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        gix::init(dir.path()).expect("init");
        let contents: String = (1..=10).map(|n| format!("line {n}\n")).collect();
        commit_file(dir.path(), "file.txt", &contents);
        Self {
            dir,
            refs: MemRefStore::default(),
            objects: ObjectStore::default(),
            key: Keypair::from_seed(1),
        }
    }

    fn uri(&self, rel: &str) -> Url {
        Url::from_file_path(self.dir.path().join(rel)).expect("file uri")
    }

    fn actor(&self) -> gix::actor::Signature {
        gix::actor::Signature {
            name: "jdc".into(),
            email: "jdc@ents.test".into(),
            time: gix::date::Time {
                seconds: 1_000,
                offset: 0,
            },
        }
    }

    /// Add a comment through the same library call the CLI makes
    /// (`lens.parity`), anchored to `lines` of `file.txt` against the
    /// working tree.
    fn add_comment(&self, body: &str, lines: Option<&str>) -> String {
        let new = NewComment {
            body: body.to_owned(),
            path: Some("file.txt".to_owned()),
            lines: lines.map(str::to_owned),
            rev: "HEAD".to_owned(),
            worktree: true,
            context: None,
            parent: None,
        };
        let key = &self.key;
        let sign = |payload: &[u8]| key.sign(payload);
        let identity = Identity {
            actor: self.actor(),
            sign: &sign,
        };
        let (id, _outcome) = comment::add(
            &self.refs,
            &self.objects,
            &NullEventSink,
            self.dir.path(),
            new,
            &identity,
            Mode::Advisory,
        )
        .expect("adds a comment");
        id
    }

    /// Consume the fixture into a wired [`Lens`] (the seams move in, exactly
    /// as `git ents lsp`'s composition root moves `LocalRoot`'s seams in).
    fn into_lens(self) -> (Lens<ObjectStore>, tempfile::TempDir) {
        let key = Keypair::from_seed(1);
        let signing = Signing::new(
            self.actor(),
            Box::new(move |payload| key.sign(payload)),
            self.key.public_openssh(),
        );
        let lens = Lens::new(
            Box::new(self.refs),
            self.objects,
            Box::new(NullEventSink),
            Mode::Advisory,
            signing,
            self.dir.path().to_owned(),
        );
        (lens, self.dir)
    }
}

fn commit_file(dir: &Path, path: &str, contents: &str) {
    std::fs::write(dir.join(path), contents).expect("write");
    run_git(dir, &["add", "-A"]);
    run_git(
        dir,
        &[
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-q",
            "-m",
            "seed",
        ],
    );
}

fn run_git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?} failed");
}

/// `lens.lenses`: an open comment whose anchor projects onto the document
/// surfaces as code lenses at its projected line, identifying the comment
/// and offering View/Reply/Resolve as commands. `lens.diagnostics`: the
/// same comment is also a hint-severity diagnostic at the same range.
#[test]
// @relation(lens.lenses, lens.diagnostics, scope=function, role=Verifies)
fn code_lenses_and_hint_diagnostics_surface_an_open_comment() {
    let fixture = Fixture::new();
    fixture.add_comment("this looks off by one", Some("5:5"));
    let uri = fixture.uri("file.txt");
    let (lens, _dir) = fixture.into_lens();

    let lenses = lens.code_lenses(&uri).expect("code lenses");
    assert_eq!(lenses.len(), 3, "one View/Reply/Resolve set");
    // Line 5 is 0-based line 4.
    assert_eq!(lenses[0].range.start.line, 4);
    let commands: Vec<&str> = lenses
        .iter()
        .filter_map(|lens| lens.command.as_ref().map(|c| c.command.as_str()))
        .collect();
    assert!(commands.contains(&CMD_VIEW));
    assert!(commands.contains(&"ents.reply"));
    assert!(commands.contains(&CMD_RESOLVE));
    assert!(
        lenses[0]
            .command
            .as_ref()
            .unwrap()
            .title
            .contains("off by one")
    );

    let diagnostics = lens.diagnostics(&uri).expect("diagnostics");
    assert_eq!(diagnostics.len(), 1);
    // `lens.diagnostics` is binding: hint severity, never a warning/error.
    assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::HINT));
    assert_eq!(diagnostics[0].range.start.line, 4);
}

/// `lens.hover`: hovering the anchored range returns the whole thread —
/// the root comment and its reply, bodies and authorship — as markup.
#[test]
// @relation(lens.hover, scope=function, role=Verifies)
fn hover_returns_the_full_thread() {
    let fixture = Fixture::new();
    let root = fixture.add_comment("root remark", Some("5:5"));
    // A reply, created through the same library the lens uses.
    let key = Keypair::from_seed(1);
    let sign = |payload: &[u8]| key.sign(payload);
    let identity = Identity {
        actor: fixture.actor(),
        sign: &sign,
    };
    comment::reply(
        &fixture.refs,
        &fixture.objects,
        &NullEventSink,
        &root,
        "a reply body".to_owned(),
        &identity,
        Mode::Advisory,
    )
    .expect("replies");
    let uri = fixture.uri("file.txt");
    let (lens, _dir) = fixture.into_lens();

    let hover = lens
        .hover(
            &uri,
            Position {
                line: 4,
                character: 0,
            },
        )
        .expect("hover")
        .expect("a comment is anchored at line 5");
    let HoverContents::Markup(markup) = hover.contents else {
        panic!("hover must be markup");
    };
    assert!(markup.value.contains("root remark"));
    assert!(markup.value.contains("a reply body"));
    assert!(
        markup.value.contains("jdc"),
        "authorship from the commit chain"
    );

    // Hovering an unrelated line yields nothing.
    assert!(
        lens.hover(
            &uri,
            Position {
                line: 0,
                character: 0
            }
        )
        .expect("hover")
        .is_none()
    );
}

/// `lens.compose`: a code action on a selection offers "Leave an ents
/// comment", whose command opens the template; running it writes the
/// template under `.git/` and asks the client to open that file.
#[test]
// @relation(lens.compose, scope=function, role=Verifies)
fn code_action_and_compose_open_the_template() {
    let fixture = Fixture::new();
    let uri = fixture.uri("file.txt");
    let (lens, dir) = fixture.into_lens();

    let range = Range {
        start: Position {
            line: 1,
            character: 0,
        },
        end: Position {
            line: 2,
            character: 0,
        },
    };
    let actions = lens.code_actions(&uri, range).expect("code actions");
    assert_eq!(actions.len(), 1);
    let lsp_types::CodeActionOrCommand::CodeAction(action) = &actions[0] else {
        panic!("expected a code action");
    };
    assert_eq!(action.title, "Leave an ents comment");
    let command = action.command.as_ref().expect("carries a command");
    assert_eq!(command.command, CMD_COMPOSE);

    // Running the command writes the template and asks to open it.
    let outcome = lens
        .execute_command(
            CMD_COMPOSE,
            &[json!({ "path": "file.txt", "lines": "2:2" })],
        )
        .expect("compose");
    let template = outcome.show_document.expect("opens the template");
    assert_eq!(
        template,
        dir.path().join(".git").join("ENTS_COMMENT_EDITMSG")
    );
    let written = std::fs::read_to_string(&template).expect("template written");
    assert!(written.contains("ents-compose-path: file.txt"));
    assert!(written.contains("Lines starting with '#' are ignored"));
}

/// `lens.compose` end to end: saving the template with a non-empty body
/// creates the comment (anchored to the working tree, `lens.working-tree`),
/// and it then surfaces as a code lens; an empty body aborts.
#[test]
// @relation(lens.compose, lens.working-tree, lens.parity, scope=function, role=Verifies)
fn saving_a_nonempty_body_creates_the_comment_and_empty_aborts() {
    let fixture = Fixture::new();
    let uri = fixture.uri("file.txt");
    let (lens, dir) = fixture.into_lens();
    let template = dir.path().join(".git").join("ENTS_COMMENT_EDITMSG");
    let template_uri = Url::from_file_path(&template).unwrap();

    // Start a compose targeting line 3.
    lens.execute_command(
        CMD_COMPOSE,
        &[json!({ "path": "file.txt", "lines": "3:3" })],
    )
    .expect("compose");

    // An empty save aborts: no comment, template removed.
    std::fs::write(
        &template,
        "\n# only comments here\n# ents-compose-path: file.txt\n# ents-compose-lines: 3:3\n",
    )
    .unwrap();
    lens.did_save(&template_uri).expect("save");
    assert!(lens.code_lenses(&uri).expect("lenses").is_empty());
    assert!(!template.exists(), "aborted compose removes the template");

    // Re-start and save a real body: the comment is created and surfaces.
    lens.execute_command(
        CMD_COMPOSE,
        &[json!({ "path": "file.txt", "lines": "3:3" })],
    )
    .expect("compose");
    std::fs::write(
        &template,
        "the third line is wrong\n# ignored\n# ents-compose-path: file.txt\n# ents-compose-lines: 3:3\n",
    )
    .unwrap();
    lens.did_save(&template_uri).expect("save");

    let lenses = lens.code_lenses(&uri).expect("lenses");
    assert_eq!(lenses.len(), 3, "the composed comment now surfaces");
    assert_eq!(lenses[0].range.start.line, 2, "anchored at line 3");
    assert!(
        lenses[0]
            .command
            .as_ref()
            .unwrap()
            .title
            .contains("third line is wrong")
    );
}

/// `lens.parity` + `model.comment-state`: View returns the thread, and
/// Resolve — the same library call the CLI runs — drops the comment from
/// the next publish, since only open comments surface (`lens.lenses`).
#[test]
// @relation(lens.parity, lens.lenses, scope=function, role=Verifies)
fn view_returns_the_thread_and_resolve_hides_it() {
    let fixture = Fixture::new();
    let id = fixture.add_comment("please fix", Some("5:5"));
    let uri = fixture.uri("file.txt");
    let (lens, _dir) = fixture.into_lens();

    let view = lens
        .execute_command(CMD_VIEW, &[json!(id)])
        .expect("view")
        .response
        .expect("view returns the thread");
    assert!(view.as_str().unwrap().contains("please fix"));

    // Resolve, then the open-only publish no longer shows it.
    let outcome = lens
        .execute_command(CMD_RESOLVE, &[json!(id)])
        .expect("resolve");
    assert!(outcome.refresh, "a mutation asks for a diagnostics refresh");
    assert!(lens.code_lenses(&uri).expect("lenses").is_empty());
    assert!(lens.diagnostics(&uri).expect("diags").is_empty());
}

/// `lens.working-tree`: the open buffer stands in for disk, so a comment's
/// range tracks unsaved edits — prepending two lines in the buffer shifts
/// the projected lens down by two.
#[test]
// @relation(lens.working-tree, scope=function, role=Verifies)
fn the_buffer_overrides_disk_so_ranges_track_unsaved_edits() {
    let fixture = Fixture::new();
    fixture.add_comment("watch this line", Some("5:5"));
    let uri = fixture.uri("file.txt");
    let (mut lens, _dir) = fixture.into_lens();

    // On disk the anchor is line 5 (0-based 4).
    let on_disk = lens.code_lenses(&uri).expect("lenses");
    assert_eq!(on_disk[0].range.start.line, 4);

    // The client sends a buffer with two extra lines prepended, unsaved.
    let buffer: String = std::iter::once("added a".to_owned())
        .chain(std::iter::once("added b".to_owned()))
        .chain((1..=10).map(|n| format!("line {n}")))
        .collect::<Vec<_>>()
        .join("\n");
    lens.did_open(uri.clone(), format!("{buffer}\n"));

    let shifted = lens.code_lenses(&uri).expect("lenses");
    assert_eq!(shifted[0].range.start.line, 6, "line 5 shifted to line 7");
}
