//! Integration coverage for the comment command layer: the broadened
//! `model.comment` (aboutness refused at creation, `model.comment-state`
//! transitions, `model.comment-context`/`model.comment-thread`
//! aggregation) and — with the most care, per this phase's plan row — the
//! `meta-ref.migration` guarantee that comment trees written by phase-7
//! code still read back, and are rewritten under the current struct only
//! when mutated.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "integration test: fixtures panic on setup failure"
)]

use ents_forge::comment::{self, ListFilter, NewComment};
use ents_receive::{Identity, Mode, NullEventSink, TxResult};
use ents_testutil::{Keypair, MemRefStore, ObjectStore, write_meta_entity};
use facet_git_tree::RawTree;
use gix_object::Write as _;
use gix_ref_store::RefStoreRead as _;
use rstest::rstest;

/// The exact struct phase-7 code declared for its Comment entity — the
/// on-disk encoding `meta-ref.migration` requires the broadened reader to
/// keep reading. Declared independently here (not imported) so this test
/// pins the *storage shape*, not whatever the crate's internal legacy
/// struct happens to be.
#[derive(facet::Facet)]
struct Phase7Comment {
    body: String,
    anchor: RawTree,
}

/// A throwaway on-disk repository holding one committed file — the
/// content anchors capture against — alongside the in-memory ref/object
/// fixtures every library test uses.
/// A detached signer over some bytes, returning an armored signature.
type Signer = Box<dyn Fn(&[u8]) -> String>;

struct Fixture {
    dir: tempfile::TempDir,
    refs: MemRefStore,
    objects: ObjectStore,
    sign: Signer,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(["-c", "user.name=test", "-c", "user.email=test@example.com"])
                .args(args)
                .status()
                .expect("git runs");
            assert!(status.success());
        };
        git(&["init", "-q"]);
        let contents: String = (1..=10).map(|n| format!("line {n}\n")).collect();
        std::fs::write(dir.path().join("file.txt"), contents).unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "seed"]);
        let key = Keypair::from_seed(1);
        Self {
            dir,
            refs: MemRefStore::default(),
            objects: ObjectStore::default(),
            sign: Box::new(move |payload| key.sign(payload)),
        }
    }

    fn path(&self) -> &std::path::Path {
        self.dir.path()
    }

    fn identity(&self) -> Identity<'_> {
        Identity {
            actor: gix::actor::Signature {
                name: "test".into(),
                email: "test@ents.test".into(),
                time: gix::date::Time {
                    seconds: 1_000,
                    offset: 0,
                },
            },
            sign: &*self.sign,
        }
    }

    fn draft(&self) -> NewComment {
        NewComment {
            body: "looks off by one".to_owned(),
            path: Some("file.txt".to_owned()),
            lines: Some("3:4".to_owned()),
            rev: "HEAD".to_owned(),
            worktree: false,
            context: None,
            parent: None,
        }
    }

    fn add(&self, draft: NewComment) -> String {
        let (id, outcome) = comment::add(
            &self.refs,
            &self.objects,
            &NullEventSink,
            self.path(),
            draft,
            &self.identity(),
            Mode::Advisory,
        )
        .expect("adds");
        assert_eq!(outcome.result, TxResult::Applied);
        id
    }

    /// Seed a comment ref exactly as phase-7 code wrote one: the bare
    /// `{body, anchor}` tree under `refs/meta/comments/<id>`.
    fn seed_phase7(&self, id: &str, body: &str) -> gix_hash::ObjectId {
        let anchor = ents_anchor::capture(
            &gix::open(self.path()).expect("opens"),
            "HEAD",
            "file.txt",
            None,
        )
        .expect("captures");
        let anchor_tree =
            facet_git_tree::serialize_into(&anchor, &self.objects).expect("serializes");
        let legacy = Phase7Comment {
            body: body.to_owned(),
            anchor: RawTree::new(anchor_tree),
        };
        let name = ents_model::namespace::comment_ref(id).expect("valid");
        write_meta_entity(&self.refs, &self.objects, name, &legacy, None, 500)
    }
}

// ---------------------------------------------------------------------
// meta-ref.migration: phase-7 trees still read; mutation rewrites.
// ---------------------------------------------------------------------

/// A pre-migration ref's tip reads back through every read surface —
/// list, show — mapping to state `open` with no context or parent.
// @relation(meta-ref.migration, scope=function, role=Verifies)
#[rstest]
fn phase7_comment_refs_still_read_back() {
    let fixture = Fixture::new();
    fixture.seed_phase7("legacy1", "written by phase-7 code");

    let listed = comment::list(&fixture.refs, &fixture.objects).expect("lists");
    assert_eq!(listed.len(), 1);
    let (id, read) = &listed[0];
    assert_eq!(id, "legacy1");
    assert_eq!(read.body, "written by phase-7 code");
    assert_eq!(read.state, "open");
    assert_eq!(read.context, None);
    assert_eq!(read.parent, None);

    let (shown, projected) = comment::show(
        &fixture.refs,
        &fixture.objects,
        fixture.path(),
        "legacy1",
        "HEAD",
        false,
    )
    .expect("shows");
    assert_eq!(shown.body, "written by phase-7 code");
    let (anchor, projection) = projected.expect("legacy comments always carry an anchor");
    assert_eq!(anchor.path, "file.txt");
    assert_eq!(projection, ents_anchor::Projection::Current);
}

/// Mutating a pre-migration ref rewrites its tree under the current
/// struct as a commit on top of the old tip (`meta-ref.migration`):
/// the new tip deserializes directly as the broadened `Comment`, its
/// parent is the untouched legacy commit, and nothing rewrote history.
// @relation(meta-ref.migration, model.comment-state, scope=function, role=Verifies)
#[rstest]
fn mutating_a_phase7_ref_migrates_it_on_top_of_the_old_tip() {
    use gix_object::Find as _;

    let fixture = Fixture::new();
    let old_tip = fixture.seed_phase7("legacy1", "written by phase-7 code");

    let outcome = comment::resolve(
        &fixture.refs,
        &fixture.objects,
        &NullEventSink,
        "legacy1",
        &fixture.identity(),
        Mode::Advisory,
    )
    .expect("resolves");
    assert_eq!(outcome.result, TxResult::Applied);

    let name = ents_model::namespace::comment_ref("legacy1").expect("valid");
    let new_tip = fixture
        .refs
        .get(name.as_ref())
        .expect("readable")
        .expect("set");
    assert_ne!(new_tip, old_tip);

    // The new tip's tree is the *current* encoding, no fallback needed...
    let mut buf = Vec::new();
    let data = fixture
        .objects
        .try_find(&new_tip, &mut buf)
        .expect("readable")
        .expect("present");
    let commit = gix_object::CommitRef::from_bytes(data.data, new_tip.kind()).expect("parses");
    let migrated: ents_forge::comment::Comment =
        facet_git_tree::deserialize(&commit.tree(), &fixture.objects)
            .expect("the migrated tip deserializes directly as the broadened struct");
    assert_eq!(migrated.state, "resolved");
    assert_eq!(migrated.body, "written by phase-7 code");
    assert!(migrated.anchor.is_some());

    // ...and it sits on top of the old tip: history keeps the old
    // encoding as archive, nothing was rewritten or deleted.
    let parents: Vec<_> = commit.parents().collect();
    assert_eq!(parents, vec![old_tip]);
}

/// The legacy fallback holds for any body content, not just the fixture
/// string — old-shape trees over an unenumerable input space read back
/// with the same mapping.
// @relation(meta-ref.migration, scope=function, role=Verifies)
#[test]
fn any_phase7_tree_reads_back_mapped_to_open() {
    let runner = proptest::test_runner::Config::with_cases(64);
    proptest::proptest!(runner, |(body in proptest::prelude::any::<String>())| {
        let objects = ObjectStore::default();
        let refs = MemRefStore::default();
        let anchor_tree = objects
            .write(&gix_object::Tree { entries: vec![] })
            .expect("tree");
        let legacy = Phase7Comment {
            body: body.clone(),
            anchor: RawTree::new(anchor_tree),
        };
        let name = ents_model::namespace::comment_ref("p").expect("valid");
        write_meta_entity(&refs, &objects, name, &legacy, None, 500);

        let listed = comment::list(&refs, &objects).expect("lists");
        proptest::prop_assert_eq!(listed.len(), 1);
        let read = &listed[0].1;
        proptest::prop_assert_eq!(&read.body, &body);
        proptest::prop_assert_eq!(&read.state, "open");
        proptest::prop_assert_eq!(read.anchor.as_ref(), Some(&RawTree::new(anchor_tree)));
        proptest::prop_assert_eq!(&read.context, &None);
        proptest::prop_assert_eq!(&read.parent, &None);
    });
}

// ---------------------------------------------------------------------
// model.comment: aboutness is required at creation, never by the gate.
// ---------------------------------------------------------------------

/// The library refuses a comment about nothing and malformed aboutness
/// arguments; every well-formed combination is accepted.
// @relation(model.comment, model.comment-context, scope=function, role=Verifies)
#[rstest]
#[case::about_nothing(None, None, None, false)]
#[case::lines_without_a_path(None, Some("issues/42"), Some("3:4"), false)]
#[case::bad_context(None, Some("not a ref\u{7f}"), None, false)]
#[case::context_only(None, Some("issues/42"), None, true)]
#[case::anchored(Some("file.txt"), None, None, true)]
#[case::anchored_and_contextual(Some("file.txt"), Some("reviews/7"), None, true)]
fn add_refuses_a_comment_about_nothing(
    #[case] path: Option<&str>,
    #[case] context: Option<&str>,
    #[case] lines: Option<&str>,
    #[case] accepted: bool,
) {
    let fixture = Fixture::new();
    let draft = NewComment {
        body: "b".to_owned(),
        path: path.map(str::to_owned),
        lines: lines.map(str::to_owned),
        rev: "HEAD".to_owned(),
        worktree: false,
        context: context.map(str::to_owned),
        parent: None,
    };
    let result = comment::add(
        &fixture.refs,
        &fixture.objects,
        &NullEventSink,
        fixture.path(),
        draft,
        &fixture.identity(),
        Mode::Advisory,
    );
    match (accepted, result) {
        (true, Ok((_, outcome))) => assert_eq!(outcome.result, TxResult::Applied),
        (false, Err(error)) => assert!(matches!(error, ents_forge::Error::InvalidArgument(_))),
        (expected, got) => panic!("expected accepted={expected}, got {got:?}"),
    }
}

/// A reply's parent must exist when the reply is created
/// (`model.comment-thread`) — both through `reply` and through `add
/// --parent`.
// @relation(model.comment-thread, scope=function, role=Verifies)
#[rstest]
fn a_reply_to_a_missing_parent_is_refused() {
    let fixture = Fixture::new();
    let error = comment::reply(
        &fixture.refs,
        &fixture.objects,
        &NullEventSink,
        "no-such-id",
        "reply".to_owned(),
        &fixture.identity(),
        Mode::Advisory,
    )
    .expect_err("refused");
    assert!(matches!(error, ents_forge::Error::NotFound { .. }));

    let mut draft = fixture.draft();
    draft.parent = Some("no-such-id".to_owned());
    let error = comment::add(
        &fixture.refs,
        &fixture.objects,
        &NullEventSink,
        fixture.path(),
        draft,
        &fixture.identity(),
        Mode::Advisory,
    )
    .expect_err("refused");
    assert!(matches!(error, ents_forge::Error::NotFound { .. }));
}

// ---------------------------------------------------------------------
// model.comment-state: resolve and reopen are ordinary ref mutations.
// ---------------------------------------------------------------------

/// A new comment opens `open`; resolve records `resolved`; reopen records
/// `open` again — three commits on one ref, never a deletion.
// @relation(model.comment-state, scope=function, role=Verifies)
#[rstest]
fn resolve_and_reopen_advance_the_same_ref() {
    let fixture = Fixture::new();
    let id = fixture.add(fixture.draft());
    let state = |fixture: &Fixture| {
        comment::list(&fixture.refs, &fixture.objects).expect("lists")[0]
            .1
            .state
            .clone()
    };
    assert_eq!(state(&fixture), "open");

    let outcome = comment::resolve(
        &fixture.refs,
        &fixture.objects,
        &NullEventSink,
        &id,
        &fixture.identity(),
        Mode::Advisory,
    )
    .expect("resolves");
    assert_eq!(outcome.result, TxResult::Applied);
    assert_eq!(state(&fixture), "resolved");

    let outcome = comment::reopen(
        &fixture.refs,
        &fixture.objects,
        &NullEventSink,
        &id,
        &fixture.identity(),
        Mode::Advisory,
    )
    .expect("reopens");
    assert_eq!(outcome.result, TxResult::Applied);
    assert_eq!(state(&fixture), "open");
}

// ---------------------------------------------------------------------
// model.comment-context / model.comment-thread: threads are aggregation
// queries over decomposed refs.
// ---------------------------------------------------------------------

/// `thread` aggregates the comments naming a context plus every reply
/// reachable through parent links — a reply repeats neither anchor nor
/// context, and no entity stored a list of anything.
// @relation(model.comment-context, model.comment-thread, scope=function, role=Verifies)
#[rstest]
fn a_thread_aggregates_context_roots_and_their_replies() {
    let fixture = Fixture::new();
    let mut root_draft = fixture.draft();
    root_draft.context = Some("reviews/7".to_owned());
    let root = fixture.add(root_draft);
    let (reply, outcome) = comment::reply(
        &fixture.refs,
        &fixture.objects,
        &NullEventSink,
        &root,
        "agreed".to_owned(),
        &fixture.identity(),
        Mode::Advisory,
    )
    .expect("replies");
    assert_eq!(outcome.result, TxResult::Applied);
    // A second-level reply, and an unrelated comment that must stay out.
    let (nested, _) = comment::reply(
        &fixture.refs,
        &fixture.objects,
        &NullEventSink,
        &reply,
        "further".to_owned(),
        &fixture.identity(),
        Mode::Advisory,
    )
    .expect("replies");
    let mut unrelated = fixture.draft();
    unrelated.context = Some("issues/9".to_owned());
    fixture.add(unrelated);

    let thread = comment::thread(&fixture.refs, &fixture.objects, "reviews/7").expect("aggregates");
    let mut ids: Vec<_> = thread.iter().map(|(id, _)| id.clone()).collect();
    ids.sort();
    let mut expected = vec![root.clone(), reply.clone(), nested.clone()];
    expected.sort();
    assert_eq!(ids, expected);

    // The reply carried no anchor and no context of its own — aboutness
    // is inherited from the thread root.
    let replied = thread
        .iter()
        .find(|(id, _)| *id == reply)
        .map(|(_, c)| c)
        .expect("present");
    assert_eq!(replied.anchor, None);
    assert_eq!(replied.context, None);
    assert_eq!(replied.parent, Some(root));
}

// ---------------------------------------------------------------------
// lens.parity: the projected listing is one library call.
// ---------------------------------------------------------------------

/// `list_projected` filters by state and context and projects each anchor
/// onto the working tree when asked — the exact call the CLI's
/// machine-readable form and the editor lens both consume.
// @relation(lens.parity, anchor.working-tree, scope=function, role=Verifies)
#[rstest]
fn list_projected_filters_and_projects_onto_the_working_tree() {
    let fixture = Fixture::new();
    let anchored = fixture.add(fixture.draft());
    let mut contextual = fixture.draft();
    contextual.path = None;
    contextual.lines = None;
    contextual.context = Some("issues/42".to_owned());
    let unanchored = fixture.add(contextual);
    comment::resolve(
        &fixture.refs,
        &fixture.objects,
        &NullEventSink,
        &unanchored,
        &fixture.identity(),
        Mode::Advisory,
    )
    .expect("resolves");

    // Dirty the working tree above the anchored range: the worktree
    // projection relocates while a HEAD projection would say Current.
    let dirty: String = std::iter::once("inserted\n".to_owned())
        .chain((1..=10).map(|n| format!("line {n}\n")))
        .collect();
    std::fs::write(fixture.path().join("file.txt"), dirty).unwrap();

    let open_only = comment::list_projected(
        &fixture.refs,
        &fixture.objects,
        fixture.path(),
        true,
        &ListFilter {
            state: Some("open".to_owned()),
            context: None,
        },
    )
    .expect("lists");
    assert_eq!(open_only.len(), 1);
    assert_eq!(open_only[0].id, anchored);
    assert_eq!(
        open_only[0].projection,
        Some(ents_anchor::Projection::Relocated {
            path: "file.txt".to_owned(),
            lines: Some(ents_anchor::LineRange { start: 4, end: 5 }),
        })
    );

    let by_context = comment::list_projected(
        &fixture.refs,
        &fixture.objects,
        fixture.path(),
        true,
        &ListFilter {
            state: None,
            context: Some("issues/42".to_owned()),
        },
    )
    .expect("lists");
    assert_eq!(by_context.len(), 1);
    assert_eq!(by_context[0].id, unanchored);
    assert_eq!(by_context[0].projection, None, "no anchor, no projection");
}

/// `--worktree` end to end at the library layer: a comment anchored to
/// dirty, uncommitted content is Current against the working tree and
/// survives the content being discarded (its content is embedded).
// @relation(anchor.working-tree, model.comment, scope=function, role=Verifies)
#[rstest]
fn a_worktree_anchored_comment_tracks_the_dirty_file() {
    let fixture = Fixture::new();
    let dirty: String = (1..=10)
        .map(|n| {
            if n == 5 {
                "line five\n".to_owned()
            } else {
                format!("line {n}\n")
            }
        })
        .collect();
    std::fs::write(fixture.path().join("file.txt"), &dirty).unwrap();

    let mut draft = fixture.draft();
    draft.worktree = true;
    draft.lines = Some("5".to_owned());
    let id = fixture.add(draft);

    let (_, projected) = comment::show(
        &fixture.refs,
        &fixture.objects,
        fixture.path(),
        &id,
        "HEAD",
        true,
    )
    .expect("shows");
    let (anchor, projection) = projected.expect("anchored");
    assert_eq!(ents_anchor::snippet(&anchor).unwrap(), "line five\n");
    assert_eq!(projection, ents_anchor::Projection::Current);

    // Discard the dirty content: the anchor's own text still reads back
    // (embedded), and the worktree projection reports the region edited.
    let git = std::process::Command::new("git")
        .arg("-C")
        .arg(fixture.path())
        .args(["checkout", "--", "file.txt"])
        .status()
        .expect("git runs");
    assert!(git.success());
    let (_, projected) = comment::show(
        &fixture.refs,
        &fixture.objects,
        fixture.path(),
        &id,
        "HEAD",
        true,
    )
    .expect("shows");
    let (anchor, projection) = projected.expect("anchored");
    assert_eq!(ents_anchor::snippet(&anchor).unwrap(), "line five\n");
    assert_eq!(
        projection,
        ents_anchor::Projection::Outdated {
            path: "file.txt".to_owned(),
        }
    );
}
