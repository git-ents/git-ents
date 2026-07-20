//! End-to-end coverage of `git ents login` (`roots.web-signin`) against
//! the real hosted-shaped web state served on a real loopback socket:
//! the same `ents_web::router` the Fly deployment proxies to, the same
//! `commands::login::run` a member's own machine executes. This doubles
//! as the hosted-mount integration test — `build_hosted_state` wired and
//! served end to end, no container needed.
#![allow(clippy::expect_used, reason = "integration test")]

use std::path::Path;
use std::process::Command;

use git_ents::root::LocalRoot;

/// A bare repository with `username`'s freshly-written key enrolled,
/// returning the key path.
fn enrolled_bare(dir: &Path, username: &str, seed: u8) -> std::path::PathBuf {
    let bare = dir.join("repo.git");
    let output = Command::new("git")
        .args(["init", "--bare"])
        .arg(&bare)
        .output()
        .expect("git runs");
    assert!(output.status.success(), "{output:?}");

    use ssh_key::private::{Ed25519Keypair, KeypairData};
    let key_path = dir.join(format!("key_{username}"));
    let pair = Ed25519Keypair::from_seed(&[seed; 32]);
    let key = ssh_key::PrivateKey::new(KeypairData::from(pair), username).expect("well-formed");
    key.write_openssh_file(&key_path, ssh_key::LineEnding::LF)
        .expect("writes");

    let local = LocalRoot::open(&bare).expect("opens");
    git_ents::commands::members::add(&local, username, None, Some(key_path.clone()))
        .expect("enrolls");
    key_path
}

// @relation(roots.web-signin, roots.single-node-hosted, scope=function, role=Verifies)
#[tokio::test(flavor = "multi_thread")]
async fn git_ents_login_signs_a_hosted_browser_session_in() {
    let dir = tempfile::tempdir().expect("tempdir");
    let server_key = enrolled_bare(dir.path(), "server", 42);
    // Enroll the human member too, with their own distinct key.
    use ssh_key::private::{Ed25519Keypair, KeypairData};
    let member_key = dir.path().join("key_joey");
    let pair = Ed25519Keypair::from_seed(&[7; 32]);
    let key = ssh_key::PrivateKey::new(KeypairData::from(pair), "joey").expect("well-formed");
    key.write_openssh_file(&member_key, ssh_key::LineEnding::LF)
        .expect("writes");
    let bare = dir.path().join("repo.git");
    let local = LocalRoot::open(&bare).expect("opens");
    git_ents::commands::members::add(&local, "joey", None, Some(member_key.clone()))
        .expect("enrolls");

    // Bind first so the realm's host names the real ephemeral port —
    // `git ents login` refuses a host disagreement by design.
    let listener = ents_web::bind("127.0.0.1:0".parse().expect("addr"))
        .await
        .expect("binds");
    let host = listener.local_addr().expect("bound").to_string();
    let root = git_ents::root::HostedRoot::open(&bare).expect("opens");
    let state = git_ents::commands::serve::build_hosted_state(root, server_key, host.clone())
        .expect("boots");
    tokio::spawn(ents_web::serve_on(listener, state));

    let url = format!("http://{host}");
    let outcome = tokio::task::spawn_blocking(move || {
        // The browser half: GET /login mints a session and displays the
        // one-time code.
        let agent = ureq::agent();
        let mut page = agent
            .get(format!("{url}/login"))
            .call()
            .expect("login page");
        let cookie = page
            .headers()
            .get("set-cookie")
            .expect("a fresh session cookie")
            .to_str()
            .expect("ascii")
            .split(';')
            .next()
            .expect("cookie pair")
            .to_owned();
        let body = page.body_mut().read_to_string().expect("html");
        let code: String = body
            .split(&format!("{host} "))
            .nth(1)
            .expect("the page displays the login command")
            .chars()
            .take(9)
            .collect();

        // The CLI half: the real command, against the real socket.
        let mut out = Vec::new();
        git_ents::commands::login::run(&url, &code, Some(member_key), &mut out).expect("signs in");
        let printed = String::from_utf8(out).expect("utf8");
        assert!(
            printed.contains("authenticated as joey"),
            "reports the member: {printed}"
        );

        // The browser half again: the same session now reads signed in.
        let mut page = agent
            .get(format!("{url}/login"))
            .header("cookie", &cookie)
            .call()
            .expect("login page");
        let body = page.body_mut().read_to_string().expect("html");
        assert!(
            body.contains("Signed in as") && body.contains("joey"),
            "the browser session is authenticated: {body}"
        );
    })
    .await;
    outcome.expect("blocking half succeeds");
}
