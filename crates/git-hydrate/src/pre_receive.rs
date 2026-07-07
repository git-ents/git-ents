//! The `pre-receive` hook body for a hydration-configured repository
//! (`docs/scale-out.adoc`, WS0's write path): apply the push through
//! [`git_protocol::native::NativeBackend::receive`] against
//! [`crate::resolver::PostgresResolver`], then log the accepted push's
//! [`git_protocol::CorpusEntry`] for later replay (WS2's seed corpus).

use std::io::{Cursor, Read as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use git_backend::{Expected, PackStream, RefEdit, RefName};
use git_protocol::attestation::{OpSigner, SshOpSigner};
use git_protocol::native::NativeBackend;
use git_protocol::{
    CorpusEntry, IngestPack as _, PushCertificate, PushOutcome, PushRequest, RepoId,
};
use gix_hash::ObjectId;

use crate::config::HydrateConfig;
use crate::resolver::PostgresResolver;

/// One ref update as git hands it to `pre-receive` on stdin.
struct RefUpdate {
    name: RefName,
    old: Option<ObjectId>,
    new: Option<ObjectId>,
}

impl RefUpdate {
    fn to_ref_edit(&self) -> RefEdit {
        RefEdit {
            name: self.name.clone(),
            expected: match self.old {
                Some(oid) => Expected::MustExistAndMatch(oid),
                None => Expected::MustNotExist,
            },
            new: self.new,
        }
    }
}

/// Run the hook: read the push git is about to apply, commit it through
/// the durable stores, and log its corpus entry.
///
/// `op_signing_key` signs the accepted push's server op record; `None`
/// rejects every push closed (mirrors `git_ents_server::native_git`'s own
/// rule: no signing key configured, no accepted push — reads are
/// unaffected). `config` names the Postgres/blob-store pair this
/// repository hydrates from and writes through.
///
/// # Errors
///
/// Returns `Err(reason)` — the caller prints `reason` to stderr and exits
/// non-zero, rejecting the whole push — if the ref updates or push
/// certificate cannot be read, the incoming pack cannot be built, or the
/// push itself is rejected (failed attestation, failed connectivity, or a
/// failed compare-and-swap against Postgres).
pub fn run(config: &HydrateConfig, op_signing_key: Option<&Path>) -> Result<(), String> {
    let repo_path =
        std::env::current_dir().map_err(|error| format!("cannot resolve repository: {error}"))?;
    let repo_id = repo_id_for(&repo_path);

    let updates = read_ref_updates()?;
    if updates.is_empty() {
        return Ok(());
    }

    let roots: Vec<ObjectId> = updates.iter().filter_map(|update| update.new).collect();
    let pack_bytes = build_pack(&repo_path, &roots)?;

    let cert_text = read_push_cert(&repo_path)?;
    let cert_bytes = cert_text
        .as_deref()
        .map(str::as_bytes)
        .unwrap_or_default()
        .to_vec();
    let cert_oid =
        gix_object::compute_hash(gix_hash::Kind::Sha1, gix_object::Kind::Blob, &cert_bytes)
            .map_err(|error| format!("could not hash push certificate: {error}"))?;

    let signer: Arc<dyn OpSigner> = match op_signing_key {
        Some(key) => Arc::new(SshOpSigner::new(key.to_path_buf())),
        // No signing key: op records fail to sign, so every otherwise
        // acceptable push is rejected — fail-closed, since an accepted
        // push without its op record breaks the "universal server op
        // record" rule.
        None => Arc::new(SshOpSigner::new(PathBuf::from("/dev/null"))),
    };
    let resolver = PostgresResolver::new(config.clone(), repo_path.clone());
    let backend = NativeBackend::new(resolver, signer);

    let ref_edits: Vec<RefEdit> = updates.iter().map(RefUpdate::to_ref_edit).collect();
    let push = PushRequest {
        repo: RepoId::new(repo_id.clone()),
        ref_edits,
        pack: PackStream::new(Cursor::new(pack_bytes.clone())),
        push_cert: cert_text.clone().map(PushCertificate::new),
    };

    match backend.receive(push).map_err(|error| error.to_string())? {
        PushOutcome::Accepted { applied, .. } => {
            let entry =
                CorpusEntry::new(cert_text.is_some().then_some(cert_oid), applied, pack_bytes);
            log_corpus_entry(config, &repo_id, &entry);
            Ok(())
        }
        PushOutcome::Rejected { reason } => Err(reason),
    }
}

/// This repository's id, relative to `$GIT_PROJECT_ROOT` when set (the
/// same env var `git-ents-server`'s CGI gateway hands every backend
/// invocation, inherited down through `receive-pack` to this hook) —
/// otherwise the repository's own directory name, so the hook still runs
/// (against a single-repo id) outside that server.
fn repo_id_for(repo_path: &Path) -> String {
    if let Ok(root) = std::env::var("GIT_PROJECT_ROOT")
        && let Ok(relative) = repo_path.strip_prefix(root)
        && !relative.as_os_str().is_empty()
    {
        return relative.to_string_lossy().replace('\\', "/");
    }
    repo_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| repo_path.to_string_lossy().into_owned())
}

/// Read the ref updates git hands `pre-receive` on stdin: `<old> <new>
/// <refname>` per line.
fn read_ref_updates() -> Result<Vec<RefUpdate>, String> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .map_err(|error| format!("could not read ref updates: {error}"))?;

    let null = ObjectId::null(gix_hash::Kind::Sha1);
    let mut updates = Vec::new();
    for line in input.lines() {
        let mut parts = line.split_whitespace();
        let (Some(old_hex), Some(new_hex), Some(name)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let old = ObjectId::from_hex(old_hex.as_bytes())
            .map_err(|error| format!("bad old oid {old_hex:?}: {error}"))?;
        let new = ObjectId::from_hex(new_hex.as_bytes())
            .map_err(|error| format!("bad new oid {new_hex:?}: {error}"))?;
        updates.push(RefUpdate {
            name: RefName::new(name),
            old: (old != null).then_some(old),
            new: (new != null).then_some(new),
        });
    }
    Ok(updates)
}

/// The push certificate git verified the nonce of, read from the blob
/// `$GIT_PUSH_CERT` names — `None` when the push carried no certificate at
/// all (only acceptable during the bootstrap window, which
/// `NativeBackend::receive`'s attestation check enforces).
///
/// # Errors
///
/// Returns `Err` if a certificate was sent but its anti-replay nonce did
/// not validate, or its blob cannot be read.
fn read_push_cert(repo: &Path) -> Result<Option<String>, String> {
    let Some(oid) = std::env::var("GIT_PUSH_CERT")
        .ok()
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    if std::env::var("GIT_PUSH_CERT_NONCE_STATUS").as_deref() != Ok("OK") {
        return Err("push certificate nonce was missing or stale".to_owned());
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["cat-file", "blob", &oid])
        .output()
        .map_err(|error| format!("could not read push certificate: {error}"))?;
    if !output.status.success() {
        return Err("could not read the push certificate from the object store".to_owned());
    }
    String::from_utf8(output.stdout)
        .map(Some)
        .map_err(|_invalid| "push certificate is not valid UTF-8".to_owned())
}

/// Build the pack introducing every object reachable from `roots` that
/// `repo` (plus its inherited quarantine — `$GIT_OBJECT_DIRECTORY`/
/// `$GIT_ALTERNATE_OBJECT_DIRECTORIES`, set by `receive-pack` for this
/// very hook) doesn't already have, by shelling out to `git rev-list`/`git
/// pack-objects` exactly as a real client push transmits one. An empty
/// `roots` (a batch of pure ref deletions) still needs a valid, empty pack.
fn build_pack(repo: &Path, roots: &[ObjectId]) -> Result<Vec<u8>, String> {
    if roots.is_empty() {
        return git_protocol::pack::build_pack(&[]).map_err(|error| error.to_string());
    }
    let mut rev_list = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-list", "--objects"])
        .args(roots.iter().map(|oid| oid.to_hex().to_string()))
        .args(["--not", "--all"])
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not spawn git rev-list: {error}"))?;
    let rev_list_stdout = rev_list
        .stdout
        .take()
        .ok_or_else(|| "git rev-list produced no stdout".to_owned())?;
    let pack_objects = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["pack-objects", "--stdout", "-q"])
        .stdin(rev_list_stdout)
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not spawn git pack-objects: {error}"))?;
    let output = pack_objects
        .wait_with_output()
        .map_err(|error| format!("git pack-objects failed: {error}"))?;
    let rev_list_status = rev_list
        .wait()
        .map_err(|error| format!("git rev-list failed: {error}"))?;
    if !rev_list_status.success() {
        return Err("git rev-list failed while building the push's pack".to_owned());
    }
    if !output.status.success() {
        return Err("git pack-objects failed while building the push's pack".to_owned());
    }
    Ok(output.stdout)
}

/// Best-effort: the push already committed to Postgres by the time this
/// runs, so a corpus-logging failure must not undo (or even report as
/// failing) an otherwise-accepted push — the same "a failure here cannot
/// undo the push" stance `git_effect::engine::post_receive` takes.
fn log_corpus_entry(config: &HydrateConfig, repo_id: &str, entry: &CorpusEntry) {
    let Ok(store) =
        refstore_postgres::PostgresRefStore::connect(&config.postgres_conninfo, repo_id.to_owned())
    else {
        return;
    };
    let _ignored = store.log_corpus_entry(entry);
}
