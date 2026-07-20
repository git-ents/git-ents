//! End-to-end coverage of the single-node hosted root
//! (`docs/development-plan.adoc`'s phase-6 row): a real bare repository
//! served by *stock git's own* `receive-pack`, with `pre-receive` /
//! `post-receive` hooks shelling to the built `git-ents` binary's `hook`
//! plumbing subcommands (`crate::hook`).
//!
//! This is the literal shape `git.ents.cloud` runs: nothing here is
//! simulated at the library-call level — every push goes through a real
//! `git push` subprocess against a real bare repository, exactly as an
//! external contributor's client would see it.
#![allow(clippy::expect_used, reason = "integration test")]

mod common;

use std::path::Path;
use std::process::Command;

use git_ents::root::LocalRoot;

/// Install `pre-receive` and `post-receive` hooks on `bare` by running the
/// real, built `git ents setup --hosted` command — not a test-harness
/// stand-in — over a subprocess, exactly how an operator deploying the
/// single-node hosted root would (`roots.single-node-hosted`).
///
/// Neither test defines an effect, so `post-receive` never has a pending
/// obligation to sign results for — meaning it is safe for the scratch
/// `HOME` this generates a key under to be cleaned up once this function
/// returns; nothing later needs to load that key again.
fn setup_hosted(bare: &Path) {
    let scratch_home = tempfile::tempdir().expect("tempdir");
    let output = Command::new(common::bin_path())
        .arg("setup")
        .arg("--hosted")
        .arg(bare)
        // Isolate from the ambient environment the same way `git()` below
        // does, but keep a real (scratch) HOME: `setup --hosted` needs
        // somewhere to generate a signing key when neither `--key` nor
        // `user.signingkey` resolves to one.
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("HOME", scratch_home.path())
        .output()
        .expect("git-ents runs");
    assert!(output.status.success(), "{output:?}");

    for hook in ["pre-receive", "post-receive"] {
        let path = bare.join("hooks").join(hook);
        assert!(path.exists(), "setup --hosted must install {hook}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
            assert!(mode & 0o111 != 0, "{hook} must be executable: {mode:o}");
        }
    }
}

fn git(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@ents.test")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@ents.test")
        // Isolate from whatever `~/.gitconfig`/`~/.ssh` the machine
        // running this test happens to have — a hosted worker's signing
        // key resolution must never depend on the ambient environment.
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env_remove("HOME")
        .output()
        .expect("git runs")
}

/// A signed enrollment commit on `refs/meta/member/<username>`, built
/// in-process against a scratch clone via `git-ents`'s own local root
/// (`LocalRoot`, exactly what `git ents members add` does) — this is how a
/// real client would produce the bytes a push transmits.
fn build_member_commit(clone: &Path, key: &Path, username: &str) {
    let root = LocalRoot::open(clone).expect("opens clone as a local root");
    git_ents::commands::members::add(&root, username, None, Some(key.to_owned()))
        .expect("builds and lands the signed enrollment commit locally");
}

/// A bootstrap enrollment pushed to the single-node hosted root round
/// trips: `setup_hosted` (`git ents setup --hosted`, `roots.single-node-hosted`)
/// installs the real hooks, then the mandatory gate (`gate.mandatory-hosted`)
/// admits the push under the bootstrap window (`gate.bootstrap`) exactly as
/// the advisory local root would, and the ref lands on the bare repository
/// for real, over a real `git push`.
// @relation(roots.local, roots.composition, roots.single-node-hosted, gate.mandatory-hosted, gate.bootstrap, scope=function, role=Verifies)
#[test]
fn bootstrap_push_round_trips_through_the_hosted_root() {
    let bare = common::Fixture::new_bare(20);
    setup_hosted(bare.path());

    let clone_dir = tempfile::tempdir().expect("tempdir");
    let clone_output = git(
        clone_dir.path(),
        &["clone", "--quiet", bare.path().to_str().expect("utf8"), "."],
    );
    assert!(clone_output.status.success(), "{clone_output:?}");

    let key = common::write_key_in(clone_dir.path(), 21);
    build_member_commit(clone_dir.path(), &key, "jdc");

    let push = git(
        clone_dir.path(),
        &["push", "origin", "refs/meta/member/jdc"],
    );
    assert!(
        push.status.success(),
        "bootstrap push must be accepted by the mandatory gate: {push:?}"
    );

    // The ref really landed on the bare (hosted) repository, not just the
    // client's own clone.
    let show = git(bare.path(), &["show-ref", "refs/meta/member/jdc"]);
    assert!(
        show.status.success(),
        "ref must exist on the hosted root: {show:?}"
    );
}

/// A second push, from a *different, unenrolled* signer, straight onto a
/// canonical meta-ref with no admin standing, must be refused by the
/// mandatory gate — and because `pre-receive` rejects the whole batch
/// before git writes anything, the object graph never lands either.
// @relation(gate.mandatory-hosted, gate.verdict-reason, scope=function, role=Verifies)
#[test]
fn unauthorized_push_is_refused_by_the_hosted_root() {
    let bare = common::Fixture::new_bare(22);
    setup_hosted(bare.path());

    // First, a legitimate admin bootstraps the repository.
    let admin_clone = tempfile::tempdir().expect("tempdir");
    let clone_output = git(
        admin_clone.path(),
        &["clone", "--quiet", bare.path().to_str().expect("utf8"), "."],
    );
    assert!(clone_output.status.success());
    let admin_key = common::write_key_in(admin_clone.path(), 23);
    build_member_commit(admin_clone.path(), &admin_key, "admin");
    let push = git(
        admin_clone.path(),
        &["push", "origin", "refs/meta/member/admin"],
    );
    assert!(push.status.success(), "{push:?}");

    // Turn on the tip invariant (`gate.epoch`): before an epoch is
    // recorded, every `refs/meta/*` update passes as `PreEpoch` — history
    // before verification is archival, not yet gated. No porcelain command
    // sets this yet (a genuine, explicitly deferred gap; see this crate's
    // final report), so this test writes the config entity the same way
    // `ents-receive`'s own doctest does: directly through
    // `ents_receive::propose_entity`, admin-signed.
    let admin_root = LocalRoot::open(admin_clone.path()).expect("opens");
    let admin_signer = git_ents::sign::Signer::load(&admin_key).expect("loads");
    let identity = ents_receive::Identity {
        actor: gix::actor::Signature {
            name: "admin".into(),
            email: "admin@ents.test".into(),
            time: gix::date::Time {
                seconds: 1_000,
                offset: 0,
            },
        },
        author: None,
        sign: &|payload| admin_signer.sign(payload),
    };
    let config_ref: gix::refs::FullName =
        ents_model::namespace::CONFIG_REF.try_into().expect("valid");
    let outcome = ents_receive::propose_entity(
        &admin_root.refs,
        &admin_root.objects,
        &admin_root.events,
        config_ref,
        &ents_gate::Config { epoch: Some(1_000) },
        &identity,
        "Enable the tip invariant",
        admin_root.mode(),
    )
    .expect("evaluates");
    git_ents::mutate::outcome_to_result(outcome, None).expect("admin may set the epoch");
    let push = git(admin_clone.path(), &["push", "origin", "refs/meta/config"]);
    assert!(push.status.success(), "{push:?}");

    // Now a second, unenrolled signer tries to enroll a member directly —
    // an ordinary member ref is unauthorized-namespace by default without
    // an admin doing it, so this must be refused.
    let outsider_clone = tempfile::tempdir().expect("tempdir");
    let clone_output = git(
        outsider_clone.path(),
        &["clone", "--quiet", bare.path().to_str().expect("utf8"), "."],
    );
    assert!(clone_output.status.success());
    let outsider_key = common::write_key_in(outsider_clone.path(), 24);
    // Fetch the admin's enrollment first so the local root's own gate
    // check has the current member list to read (mirrors a real client's
    // fetch-before-push).
    let fetch = git(
        outsider_clone.path(),
        &["fetch", "origin", "+refs/meta/*:refs/meta/*"],
    );
    assert!(fetch.status.success(), "{fetch:?}");
    build_member_commit(outsider_clone.path(), &outsider_key, "mallory");

    let push = git(
        outsider_clone.path(),
        &["push", "origin", "refs/meta/member/mallory"],
    );
    assert!(
        !push.status.success(),
        "an unauthorized signer's push must be refused by the mandatory gate"
    );

    let show = git(bare.path(), &["show-ref", "refs/meta/member/mallory"]);
    assert!(
        !show.status.success(),
        "a refused pre-receive push must leave no trace on the hosted root"
    );
}

/// `serve --hosted` fails closed on an unenrolled server key
/// (`roots.web-signing`: the signing key must itself be an enrolled
/// member) and boots once the key is enrolled, with the enrolled
/// username as the serving identity's label.
// @relation(roots.web-signing, roots.single-node-hosted, scope=function, role=Verifies)
#[test]
fn hosted_serve_boots_only_with_an_enrolled_server_key() {
    use ssh_key::private::{Ed25519Keypair, KeypairData};

    let dir = tempfile::tempdir().expect("tempdir");
    let bare = dir.path().join("repo.git");
    let output = Command::new("git")
        .args(["init", "--bare"])
        .arg(&bare)
        .output()
        .expect("git runs");
    assert!(output.status.success(), "{output:?}");

    let key_path = dir.path().join("hosted_signing_key");
    let pair = Ed25519Keypair::from_seed(&[42; 32]);
    let key = ssh_key::PrivateKey::new(KeypairData::from(pair), "server").expect("well-formed");
    key.write_openssh_file(&key_path, ssh_key::LineEnding::LF)
        .expect("writes");

    let root = git_ents::root::HostedRoot::open(&bare).expect("opens");
    let refused = git_ents::commands::serve::build_hosted_state(
        root,
        key_path.clone(),
        "ents.test".to_owned(),
    );
    assert!(
        refused.is_err(),
        "an unenrolled server key must refuse to boot"
    );

    let local = LocalRoot::open(&bare).expect("opens");
    git_ents::commands::members::add(&local, "server", None, Some(key_path.clone()))
        .expect("enrolls the server key");

    let root = git_ents::root::HostedRoot::open(&bare).expect("opens");
    let state =
        git_ents::commands::serve::build_hosted_state(root, key_path, "ents.test".to_owned())
            .expect("boots once enrolled");
    assert_eq!(state.identity.label(), "server");
}
