#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    reason = "integration test binary"
)]

//! End-to-end coverage for authenticated browser edits: signing in with a web
//! key, then saving a settings change that must travel through the real
//! `pre-receive` gate as a signed push before it lands on `refs/meta/config`.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_git-ents-server");

#[test]
fn a_member_edits_settings_through_the_browser() {
    let env = Server::start();
    let bare = env.create_repo("repo.git");
    let key = keygen(env.scratch(), "web");
    env.add_member(&bare, "alice", &pubkey(&key));

    // Sign in with the web key; the server derives its public half and opens a
    // session whose cookie we carry from here on.
    let signin = env.post("/login", "", &form(&[("private_key", &read(&key))]));
    assert_eq!(signin.status, 303, "sign-in should redirect");
    let cookie = signin.session_cookie().unwrap();

    // The settings page now offers an edit form; read its CSRF token.
    let page = env.get("/repo.git/settings", &cookie);
    assert!(
        page.body.contains("name=\"csrf\""),
        "edit form should render"
    );
    let csrf = page.csrf().unwrap();

    // Save a change to every General field.
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

    // And the gate really moved the ref: a fresh signed commit now sits on it.
    assert!(
        git(
            &bare,
            &["rev-parse", "--verify", "--quiet", "refs/meta/config"]
        )
        .is_some(),
        "refs/meta/config should exist after the edit"
    );
}

#[test]
fn an_edit_without_a_valid_csrf_token_is_refused() {
    let env = Server::start();
    let bare = env.create_repo("repo.git");
    let key = keygen(env.scratch(), "web");
    env.add_member(&bare, "alice", &pubkey(&key));

    let cookie = env
        .post("/login", "", &form(&[("private_key", &read(&key))]))
        .session_cookie()
        .unwrap();

    let edit = env.post(
        "/repo.git/settings",
        &cookie,
        &form(&[("csrf", "not-the-token"), ("description", "sneaky")]),
    );
    assert_eq!(edit.status, 200, "a bad-CSRF edit should not redirect");
    assert!(
        env.get("/repo.git/settings", &cookie)
            .body
            .contains("name=\"csrf\"")
            && !page_description_is(&env, &cookie, "sneaky"),
        "the description must be unchanged"
    );
}

#[test]
fn a_non_member_is_not_offered_an_edit_form() {
    let env = Server::start();
    let bare = env.create_repo("repo.git");
    let member = keygen(env.scratch(), "member");
    env.add_member(&bare, "alice", &pubkey(&member));

    // Sign in with a key that is *not* a member of this repo.
    let intruder = keygen(env.scratch(), "intruder");
    let cookie = env
        .post("/login", "", &form(&[("private_key", &read(&intruder))]))
        .session_cookie()
        .unwrap();

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

/// Whether the rendered settings page shows `value` as the description.
fn page_description_is(env: &Server, cookie: &str, value: &str) -> bool {
    env.get("/repo.git/settings", cookie).body.contains(value)
}

/// A running server with its data and hooks directories.
struct Server {
    child: Child,
    port: u16,
    data: tempfile::TempDir,
    scratch: tempfile::TempDir,
    _hooks: tempfile::TempDir,
}

impl Server {
    /// Start a server enforcing the signed-push gate: a nonce seed plus a hooks
    /// directory whose `pre-receive` is the compiled verifier.
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

        let port = free_port();
        let child = Command::new(BIN)
            .arg("--port")
            .arg(port.to_string())
            .arg("--data-dir")
            .arg(data.path())
            .arg("--cert-nonce-seed")
            .arg("test-seed")
            .arg("--hooks-dir")
            .arg(hooks.path())
            .spawn()
            .unwrap();
        wait_for_port(port);
        Self {
            child,
            port,
            data,
            scratch,
            _hooks: hooks,
        }
    }

    fn scratch(&self) -> &Path {
        self.scratch.path()
    }

    /// Create a bare repo by pushing an initial commit to it (auto-init), and
    /// return its on-disk path.
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

    /// Write a member ref `refs/meta/member/<username>` into `bare` directly, in
    /// the on-disk layout the loader reads: a `principal` blob, empty
    /// `valid_after`/`valid_before` subtrees, and a `trust/Keys/key` blob.
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

    fn get(&self, path: &str, cookie: &str) -> Http {
        self.request("GET", path, &[("Cookie", cookie)], "")
    }

    fn post(&self, path: &str, cookie: &str, body: &str) -> Http {
        let mut headers = vec![("Content-Type", "application/x-www-form-urlencoded")];
        if !cookie.is_empty() {
            headers.push(("Cookie", cookie));
        }
        self.request("POST", path, &headers, body)
    }

    /// Send one HTTP/1.0 request and parse the response.
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

    /// The `ents_session=<token>` pair from a `Set-Cookie` header, ready to send
    /// back as a `Cookie` value.
    fn session_cookie(&self) -> Option<String> {
        self.headers
            .iter()
            .filter(|(name, _)| name == "set-cookie")
            .find_map(|(_, value)| value.split(';').next())
            .filter(|pair| pair.starts_with("ents_session="))
            .map(str::to_owned)
    }

    /// The CSRF token rendered in the page's hidden field.
    fn csrf(&self) -> Option<String> {
        let marker = "name=\"csrf\" value=\"";
        let start = self.body.find(marker)? + marker.len();
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

fn pubkey(private: &Path) -> String {
    read(&private.with_extension("pub")).trim().to_owned()
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

/// Encode form fields as `application/x-www-form-urlencoded`.
fn form(fields: &[(&str, &str)]) -> String {
    fields
        .iter()
        .map(|(key, value)| format!("{key}={}", encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Percent-encode one form value.
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
        .stdin(Stdio::null())
        .output()
        .unwrap();
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|out| !out.is_empty() || args.first() == Some(&"update-ref"))
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
