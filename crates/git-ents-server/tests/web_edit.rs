#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    reason = "integration test binary"
)]

//! End-to-end coverage for authenticated browser edits: proving control of a
//! member key by signing a one-time challenge (the key never leaves the client),
//! then saving a settings change that travels through the real `pre-receive`
//! gate — signed with the server's own key, authored by the member — before it
//! lands on `refs/meta/config`.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_git-ents-server");
const LOGIN_NAMESPACE: &str = "git-ents-login";

#[test]
fn a_member_edits_settings_through_the_browser() {
    let env = Server::start();
    let bare = env.create_repo("repo.git");
    env.add_server_member(&bare);
    let alice = keygen(env.scratch(), "alice");
    env.add_member(&bare, "alice", &pubkey(&alice));

    let cookie = env.sign_in(&alice);

    // The settings page now offers an edit form; read its CSRF token.
    let page = env.get("/repo.git/settings", &cookie);
    assert!(
        page.body.contains("name=\"csrf\""),
        "edit form should render"
    );
    let csrf = page.field("csrf").unwrap();

    let edit = env.post(
        "/repo.git/settings",
        &cookie,
        &form(&[
            ("csrf", &csrf),
            ("description", "Edited from the browser"),
            ("homepage", "https://ents.example"),
            ("topics", "rust, git, forge"),
        ]),
    );
    assert_eq!(
        edit.status, 303,
        "a valid edit should redirect: {}",
        edit.body
    );

    // The change is reflected because it landed on `refs/meta/config`: the page
    // re-reads the ref, it is not echoing the submitted form.
    let after = env.get("/repo.git/settings", &cookie);
    assert!(
        after.body.contains("Edited from the browser"),
        "description did not land"
    );
    assert!(
        after.body.contains("https://ents.example"),
        "homepage did not land"
    );
    assert!(after.body.contains("forge"), "topics did not land");

    // "$USERNAME via Web": the commit is authored by the member and committed by
    // the server identity.
    assert_eq!(
        git(&bare, &["log", "-1", "--format=%an", "refs/meta/config"]).as_deref(),
        Some("alice"),
        "the edit should be authored by the member"
    );
    assert_eq!(
        git(&bare, &["log", "-1", "--format=%cn", "refs/meta/config"]).as_deref(),
        Some("git-ents"),
        "the committer should be the server identity"
    );
}

#[test]
fn an_edit_without_a_valid_csrf_token_is_refused() {
    let env = Server::start();
    let bare = env.create_repo("repo.git");
    env.add_server_member(&bare);
    let alice = keygen(env.scratch(), "alice");
    env.add_member(&bare, "alice", &pubkey(&alice));

    let cookie = env.sign_in(&alice);
    let edit = env.post(
        "/repo.git/settings",
        &cookie,
        &form(&[("csrf", "not-the-token"), ("description", "sneaky")]),
    );
    assert_eq!(edit.status, 200, "a bad-CSRF edit should not redirect");
    assert!(
        !env.get("/repo.git/settings", &cookie)
            .body
            .contains("sneaky"),
        "the description must be unchanged"
    );
}

#[test]
fn a_self_attested_member_is_refused_a_settings_edit() {
    let env = Server::start();
    let bare = env.create_repo("repo.git");
    env.add_server_member(&bare);
    let alice = keygen(env.scratch(), "alice");
    env.add_self_attested_member(&bare, "alice", &pubkey(&alice));

    let cookie = env.sign_in(&alice);
    let page = env.get("/repo.git/settings", &cookie);
    assert!(
        page.body.contains("name=\"csrf\""),
        "the edit form should still render for a self-attested member"
    );
    let csrf = page.field("csrf").unwrap();

    let edit = env.post(
        "/repo.git/settings",
        &cookie,
        &form(&[
            ("csrf", &csrf),
            ("description", "should not land"),
            ("homepage", ""),
            ("topics", ""),
        ]),
    );
    assert_eq!(
        edit.status, 200,
        "a self-attested member's edit should not redirect: {}",
        edit.body
    );
    assert!(
        !env.get("/repo.git/settings", &cookie)
            .body
            .contains("should not land"),
        "the description must be unchanged"
    );
}

#[test]
fn a_non_member_is_not_offered_an_edit_form() {
    let env = Server::start();
    let bare = env.create_repo("repo.git");
    env.add_server_member(&bare);
    let member = keygen(env.scratch(), "member");
    env.add_member(&bare, "alice", &pubkey(&member));

    // A real key that is simply not a member of this repo signs in fine — a
    // session proves key control, not authority.
    let intruder = keygen(env.scratch(), "intruder");
    let cookie = env.sign_in(&intruder);

    let page = env.get("/repo.git/settings", &cookie);
    assert!(
        page.body.contains("not a member"),
        "a non-member should be told they cannot edit"
    );
    assert!(
        !page.body.contains("Save changes"),
        "a non-member should not see the edit form"
    );
}

#[test]
fn a_signature_that_does_not_match_the_public_key_is_refused() {
    let env = Server::start();
    let alice = keygen(env.scratch(), "alice");
    let mallory = keygen(env.scratch(), "mallory");

    // Sign the challenge with mallory's key but claim alice's public key.
    let nonce = env.get("/login", "").field("nonce").unwrap();
    let signature = sign_nonce(&mallory, &nonce);
    let attempt = env.post(
        "/login",
        "",
        &form(&[
            ("nonce", &nonce),
            ("public_key", &pubkey(&alice)),
            ("signature", &signature),
        ]),
    );
    assert_eq!(
        attempt.status, 200,
        "a mismatched signature must not open a session"
    );
    assert!(
        attempt.session_cookie().is_none(),
        "no session cookie should be set"
    );
}

/// A running server enforcing the signed-push gate and holding a web signing key.
struct Server {
    child: Child,
    port: u16,
    data: tempfile::TempDir,
    scratch: tempfile::TempDir,
    server_key: PathBuf,
    _hooks: tempfile::TempDir,
}

impl Server {
    fn start() -> Self {
        let data = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let hooks = tempfile::tempdir().unwrap();
        let hook = hooks.path().join("pre-receive");
        std::fs::write(&hook, format!("#!/bin/sh\nexec \"{BIN}\" pre-receive\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let server_key = keygen(scratch.path(), "web-server");

        let port = free_port();
        let child = Command::new(BIN)
            .args(["--port", &port.to_string()])
            .arg("--data-dir")
            .arg(data.path())
            .args(["--cert-nonce-seed", "test-seed"])
            .arg("--hooks-dir")
            .arg(hooks.path())
            .arg("--web-signing-key")
            .arg(&server_key)
            .spawn()
            .unwrap();
        wait_for_port(port);
        Self {
            child,
            port,
            data,
            scratch,
            server_key,
            _hooks: hooks,
        }
    }

    fn scratch(&self) -> &Path {
        self.scratch.path()
    }

    /// Create a bare repo by pushing an initial commit to it (auto-init).
    fn create_repo(&self, name: &str) -> PathBuf {
        let work = self.scratch.path().join(format!("work-{name}"));
        std::fs::create_dir_all(&work).unwrap();
        run(&work, "git", &["init", "-q", "-b", "main"]);
        std::fs::write(work.join("README.md"), "hello\n").unwrap();
        run(&work, "git", &["add", "."]);
        run(
            &work,
            "git",
            &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "init"],
        );
        let url = format!("http://127.0.0.1:{}/{name}", self.port);
        run(&work, "git", &["push", "-q", &url, "main"]);
        self.data.path().join(name)
    }

    /// Add the server's own key as a member, so it may sign web edits.
    fn add_server_member(&self, bare: &Path) {
        self.add_member(bare, "web-server", &pubkey(&self.server_key));
    }

    /// Sign in with `key` via the challenge flow, returning the session cookie.
    fn sign_in(&self, key: &Path) -> String {
        let nonce = self.get("/login", "").field("nonce").unwrap();
        let signature = sign_nonce(key, &nonce);
        let response = self.post(
            "/login",
            "",
            &form(&[
                ("nonce", &nonce),
                ("public_key", &pubkey(key)),
                ("signature", &signature),
            ]),
        );
        assert_eq!(
            response.status, 303,
            "sign-in should redirect: {}",
            response.body
        );
        response.session_cookie().unwrap()
    }

    /// Write a member ref `refs/meta/member/<username>` into `bare` directly, in
    /// the on-disk layout the loader reads.
    fn add_member(&self, bare: &Path, username: &str, public_key: &str) {
        let principal = hash_object(bare, username.as_bytes());
        let key_blob = hash_object(bare, public_key.as_bytes());
        let keys = mktree(bare, &format!("100644 blob {key_blob}\tkey\n"));
        let trust = mktree(bare, &format!("040000 tree {keys}\tKeys\n"));
        let empty = mktree(bare, "");
        let root = mktree(
            bare,
            &format!(
                "100644 blob {principal}\tprincipal\n\
                 040000 tree {empty}\tvalid_after\n\
                 040000 tree {empty}\tvalid_before\n\
                 040000 tree {trust}\ttrust\n"
            ),
        );
        let commit = git(bare, &["commit-tree", &root, "-m", "member"]).unwrap();
        git(
            bare,
            &[
                "update-ref",
                &format!("refs/meta/member/{username}"),
                &commit,
            ],
        )
        .unwrap();
    }

    /// Like [`Server::add_member`], but with `provenance/SelfAttestedWeb` —
    /// the shape a member self-onboarded through the browser carries, still
    /// resting on a leaf key so the challenge-response sign-in flow works.
    fn add_self_attested_member(&self, bare: &Path, username: &str, public_key: &str) {
        let principal = hash_object(bare, username.as_bytes());
        let key_blob = hash_object(bare, public_key.as_bytes());
        let keys = mktree(bare, &format!("100644 blob {key_blob}\tkey\n"));
        let trust = mktree(bare, &format!("040000 tree {keys}\tKeys\n"));
        let empty = mktree(bare, "");
        let provenance = mktree(bare, &format!("040000 tree {empty}\tSelfAttestedWeb\n"));
        let root = mktree(
            bare,
            &format!(
                "100644 blob {principal}\tprincipal\n\
                 040000 tree {empty}\tvalid_after\n\
                 040000 tree {empty}\tvalid_before\n\
                 040000 tree {trust}\ttrust\n\
                 040000 tree {provenance}\tprovenance\n"
            ),
        );
        let commit = git(bare, &["commit-tree", &root, "-m", "member"]).unwrap();
        git(
            bare,
            &[
                "update-ref",
                &format!("refs/meta/member/{username}"),
                &commit,
            ],
        )
        .unwrap();
    }

    fn get(&self, path: &str, cookie: &str) -> Http {
        let headers: Vec<(&str, &str)> = if cookie.is_empty() {
            vec![]
        } else {
            vec![("Cookie", cookie)]
        };
        self.request("GET", path, &headers, "")
    }

    fn post(&self, path: &str, cookie: &str, body: &str) -> Http {
        let mut headers = vec![("Content-Type", "application/x-www-form-urlencoded")];
        if !cookie.is_empty() {
            headers.push(("Cookie", cookie));
        }
        self.request("POST", path, &headers, body)
    }

    fn request(&self, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Http {
        let mut request = format!("{method} {path} HTTP/1.0\r\nHost: 127.0.0.1\r\n");
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
        request.push_str(body);

        let mut stream = TcpStream::connect(format!("127.0.0.1:{}", self.port)).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).unwrap();
        Http::parse(&String::from_utf8_lossy(&raw))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.child.kill().unwrap();
        let _wait = self.child.wait();
    }
}

/// A parsed HTTP response.
struct Http {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Http {
    fn parse(raw: &str) -> Self {
        let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw, ""));
        let mut lines = head.lines();
        let status = lines
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse().ok())
            .unwrap_or(0);
        let headers = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_lowercase(), value.trim().to_owned()))
            .collect();
        Self {
            status,
            headers,
            body: body.to_owned(),
        }
    }

    fn session_cookie(&self) -> Option<String> {
        self.headers
            .iter()
            .filter(|(name, _)| name == "set-cookie")
            .find_map(|(_, value)| value.split(';').next())
            .filter(|pair| pair.starts_with("ents_session="))
            .map(str::to_owned)
    }

    /// The value of a hidden form field rendered as `name="<field>" value="…"`.
    fn field(&self, field: &str) -> Option<String> {
        let marker = format!("name=\"{field}\" value=\"");
        let start = self.body.find(&marker)? + marker.len();
        let rest = self.body.get(start..)?;
        let end = rest.find('"')?;
        rest.get(..end).map(str::to_owned)
    }
}

fn free_port() -> u16 {
    let probe = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
    port
}

fn wait_for_port(port: u16) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match TcpStream::connect(format!("127.0.0.1:{port}")) {
            Ok(_) => return,
            Err(_) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(e) => panic!("server never accepted connections: {e}"),
        }
    }
}

/// Generate an ed25519 keypair at `base/<name>`, returning the private key path.
fn keygen(base: &Path, name: &str) -> PathBuf {
    let key = base.join(name);
    let status = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", name, "-f"])
        .arg(&key)
        .status()
        .unwrap();
    assert!(status.success(), "ssh-keygen failed");
    key
}

/// Sign `nonce` under the login namespace with `key`, returning the SSHSIG.
fn sign_nonce(key: &Path, nonce: &str) -> String {
    let mut child = Command::new("ssh-keygen")
        .args(["-Y", "sign", "-n", LOGIN_NAMESPACE, "-f"])
        .arg(key)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(nonce.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "ssh-keygen -Y sign failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn pubkey(private: &Path) -> String {
    std::fs::read_to_string(private.with_extension("pub"))
        .unwrap()
        .trim()
        .to_owned()
}

/// Encode form fields as `application/x-www-form-urlencoded`.
fn form(fields: &[(&str, &str)]) -> String {
    fields
        .iter()
        .map(|(key, value)| format!("{key}={}", encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Percent-encode one form value, leaving only the unreserved set unescaped.
fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

fn run(dir: &Path, program: &str, args: &[&str]) {
    let output = Command::new(program)
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@e")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@e")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{program} {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Run `git -C bare <args>`, returning trimmed stdout on success.
fn git(bare: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(bare)
        .args(args)
        .envs(identity())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn identity() -> [(&'static str, &'static str); 4] {
    [
        ("GIT_AUTHOR_NAME", "test"),
        ("GIT_AUTHOR_EMAIL", "test@example.com"),
        ("GIT_COMMITTER_NAME", "test"),
        ("GIT_COMMITTER_EMAIL", "test@example.com"),
    ]
}

fn hash_object(bare: &Path, bytes: &[u8]) -> String {
    pipe(bare, &["hash-object", "-w", "--stdin"], bytes)
}

fn mktree(bare: &Path, spec: &str) -> String {
    pipe(bare, &["mktree"], spec.as_bytes())
}

fn pipe(bare: &Path, args: &[&str], input: &[u8]) -> String {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(bare)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "git {args:?} failed");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}
