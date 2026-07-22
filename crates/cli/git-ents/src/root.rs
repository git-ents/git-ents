//! Composition roots (`roots.composition`): the only place `git-ents`
//! wires the four seams — `RefStore`, the object store, `EventSink`, and
//! `Executor` — together.
//!
//! Two roots live in this module, per the development plan's phase-6 row:
//!
//! - [`LocalRoot`] (`roots.local`): the plain CLI, wired against whatever
//!   repository the current directory is in — loose-ref `RefStore`, the
//!   local odb, a null `EventSink`, the advisory gate, and a fixed
//!   `DockerExecutor` (there is no `--executor` flag anywhere in
//!   [`crate::cli`] to choose otherwise; local execution stays pull-only,
//!   via `git effect run`, per `effect.local-run`).
//! - [`HostedRoot`] (`roots.single-node-hosted`, the single-node hosted
//!   root the development plan's `git-ents` row describes: "loose refs and
//!   a real odb on a Fly volume, served behind git's own `receive-pack`...
//!   with an in-memory `EventSink` and a boot-time reconciliation scan, and
//!   the Sprite executor"): the same loose-ref/odb primitives as
//!   [`LocalRoot`], but the mandatory gate, an in-memory `EventSink`, and a
//!   fixed `SpriteExecutor` — wired by the `git-ents hook` plumbing
//!   subcommands ([`crate::hook`]) that git's own `receive-pack` invokes.
//!
//! Neither root is `roots.hosted` (`git-ents-server`, phase 8): that root
//! replaces the `RefStore` and object store with Postgres and Tigris and
//! is out of scope until scale forces it (`roots.honesty-test`). This
//! module's `HostedRoot` keeps git's own on-disk repository and
//! `receive-pack` as the transport, exactly as the development plan's
//! preamble describes for this phase.
//!
//! # Config isolation (`roots.config-isolation`)
//!
//! Every trait implementation is selected here, in these two structs, and
//! nowhere else: no command module reads an environment variable or git
//! config value to decide *which* `RefStore` or `Executor` to use — they
//! are only ever handed one already-constructed by a root.
//!
//! # Boundary rules this module upholds
//!
//! [`LocalRoot`] and [`HostedRoot`] are the first composition roots this
//! codebase has (every crate before phase 6 was a library, handed trait
//! objects rather than constructing them): `arch.store-composition-root`
//! ("a concrete store implementation... MUST be wired only inside a
//! composition root") and `arch.no-hosted-branch` ("a library crate MUST
//! NOT contain a branch on deployment mode") are both properties this
//! file demonstrates rather than merely states — `LocalRoot` and
//! `HostedRoot` are two distinct types, never one type with an
//! `if hosted` branch, and every command module ([`crate::commands`])
//! takes an already-constructed root, never constructing a store itself.
// @relation(roots.composition, roots.local, roots.single-node-hosted, roots.config-isolation, arch.store-composition-root, arch.no-hosted-branch, scope=file)

use std::path::{Path, PathBuf};

use ents_effect::Executor;
use ents_receive::{Mode, NullEventSink};
use gix_ref_store::LooseRefStore;

use crate::credentials::CredentialStore;
use crate::error::{Error, Result};

/// A real, on-disk object store: the repository's own odb, opened for
/// genuine reads *and* writes (`arch.no-object-store-trait`: accessed only
/// through gitoxide's own `Find`/`Write` traits, never a private one).
///
/// `gix::Repository::objects` proxies writes into an in-memory overlay by
/// default (so in-process object creation can be staged before a
/// transaction commits); a composition root that wants every write to land
/// on disk immediately calls
/// [`gix_odb::memory::Proxy::with_write_passthrough`] to strip that
/// overlay off, which is exactly what [`open_objects`] does. This is the
/// "which object directory... is the composition root's responsibility to
/// wire" `ents_receive::receive` itself defers to its caller.
pub type Objects = gix::OdbHandle;

/// Open `path`'s repository and return a real, write-through object store
/// over it (see [`Objects`]'s own doc for why `with_write_passthrough` is
/// required here).
///
/// # Errors
///
/// [`Error::Repo`] if `path` is not a git repository `gix` can open.
pub fn open_objects(path: &Path) -> Result<Objects> {
    let repo = gix::open(path)?;
    Ok(repo.objects.with_write_passthrough())
}

/// The local composition root (`roots.local`): a loose-ref `RefStore`, the
/// local odb, a null `EventSink`, the advisory gate. Wired once per CLI
/// invocation against whichever repository the current directory
/// discovers.
///
/// # Examples
///
/// ```
/// # let dir = tempfile::tempdir().expect("tempdir");
/// # gix::init(dir.path()).expect("init");
/// use git_ents::root::LocalRoot;
///
/// let root = LocalRoot::open(dir.path()).expect("opens a real repository");
/// assert_eq!(root.mode(), ents_receive::Mode::Advisory);
/// ```
pub struct LocalRoot {
    /// The repository path this root was opened against.
    pub path: PathBuf,
    /// The loose-ref `RefStore` (`arch.loose-cas-discipline`).
    pub refs: LooseRefStore,
    /// The real, on-disk object store.
    pub objects: Objects,
    /// The null `EventSink` (`roots.local`): local effect execution is
    /// pull-only, so nothing is ever enqueued here (`effect.local-run`).
    pub events: NullEventSink,
    /// The fixed `Executor` this root wires (`roots.local`): a
    /// `DockerExecutor`. Boxed because `LocalRoot` and `HostedRoot` fix
    /// different concrete backends and every command module is written
    /// against the trait, never a specific one.
    pub executor: Box<dyn Executor>,
}

impl LocalRoot {
    /// Open the local composition root against the repository at `path`.
    ///
    /// # Errors
    ///
    /// [`Error::Repo`] or [`Error::Refs`] if `path` is not a git
    /// repository, or its refs cannot be opened.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_owned();
        let refs = LooseRefStore::open(&path)?;
        let objects = open_objects(&path)?;
        Ok(Self {
            path,
            refs,
            objects,
            events: NullEventSink,
            executor: Box::new(ents_effect::DockerExecutor),
        })
    }

    /// Discover the repository from `start` upward (mirroring `git`'s own
    /// discovery), then open the local root against it.
    ///
    /// # Errors
    ///
    /// [`Error::NotARepo`] if no git repository is found at or above
    /// `start`.
    pub fn discover(start: impl AsRef<Path>) -> Result<Self> {
        let start = start.as_ref();
        let discovered = gix::discover(start).map_err(|_source| Error::NotARepo {
            path: start.to_owned(),
        })?;
        let path = discovered.workdir().unwrap_or_else(|| discovered.path());
        Self::open(path)
    }

    /// The gate policy this root runs under: always advisory
    /// (`gate.advisory-local`) — a local write is annotated, never
    /// blocked.
    #[must_use]
    pub fn mode(&self) -> Mode {
        Mode::Advisory
    }
}

/// The single-node hosted composition root: the same loose-ref/odb
/// primitives [`LocalRoot`] uses, but the mandatory gate
/// (`gate.mandatory-hosted`) — a push landing on the actual canonical
/// remote has teeth (`docs/design.adoc`: "the hosted server is not where
/// policy lives — it is the one place where the verdict has teeth") — and
/// an in-memory `EventSink` reconciled at boot
/// (`receive.reconstructible`).
///
/// This is wired by [`crate::hook`]'s plumbing subcommands, which git's own
/// `receive-pack` invokes as `pre-receive`/`post-receive` hooks; see that
/// module's doc for why the ref *write* itself is left to git's native
/// `receive-pack` rather than `ents_receive::receive`'s own
/// `RefStore::transaction` in this deployment shape.
pub struct HostedRoot {
    /// The repository path this root was opened against.
    pub path: PathBuf,
    /// The loose-ref `RefStore`.
    pub refs: LooseRefStore,
    /// The real, on-disk object store, transparently extended to also read
    /// through a `pre-receive` quarantine directory when the environment
    /// names one (`GIT_OBJECT_DIRECTORY`) — see [`QuarantineObjects`]'s own
    /// doc for why this is an in-process read chain rather than a written
    /// `info/alternates` file.
    pub objects: QuarantineObjects,
    /// The in-memory `EventSink`, reconciled at boot
    /// (`receive.reconstructible`).
    pub events: ents_receive::MemoryEventSink,
    /// The fixed `Executor` this root wires (`roots.single-node-hosted`): a
    /// `SpriteExecutor` targeting [`HOSTED_WORKER_NAME`].
    pub executor: Box<dyn Executor>,
    /// Per-member BYOK credentials (`roots.config-isolation`), read once
    /// from [`crate::credentials::CREDENTIALS_FILE_VAR`] — the seam
    /// [`crate::agent_worker::run_agent_exec`]/
    /// [`crate::plan_worker::run_agent_plan`] resolve a session's own
    /// member's credential from before injecting it into a sandbox launch.
    pub credentials: CredentialStore,
}

/// The Sprite name (and commit author name) the single-node hosted root's
/// worker uses — shared between [`HostedRoot`]'s `SpriteExecutor` and
/// [`crate::hook::post_receive`]'s result-commit author, so the two stay
/// the same identity by construction rather than by two literals staying
/// in sync by hand.
pub const HOSTED_WORKER_NAME: &str = "git-ents-hosted-worker";

impl HostedRoot {
    /// Open the hosted composition root against the repository at `path`,
    /// honoring a pre-receive quarantine object directory if the
    /// environment names one (`GIT_OBJECT_DIRECTORY`), and immediately run
    /// the boot-time reconciliation scan (`receive.reconstructible`) to
    /// populate the in-memory `EventSink` from repository state alone.
    ///
    /// # Errors
    ///
    /// [`Error::Repo`] or [`Error::Refs`] if `path` is not a git
    /// repository; [`Error::Receive`] if the reconciliation scan itself
    /// fails to read repository state; [`Error::Io`]/[`Error::InvalidArgument`]
    /// if [`crate::credentials::CREDENTIALS_FILE_VAR`] names a credentials
    /// file that cannot be read or is malformed.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_owned();
        let refs = LooseRefStore::open(&path)?;
        let objects = QuarantineObjects::open(&path)?;
        let events = ents_receive::MemoryEventSink::default();
        ents_receive::reconcile(&refs, &objects, &events)?;
        let credentials = CredentialStore::from_env()?;
        Ok(Self {
            path,
            refs,
            objects,
            events,
            executor: Box::new(ents_effect::SpriteExecutor::new(HOSTED_WORKER_NAME)),
            credentials,
        })
    }

    /// The gate policy this root runs under: always mandatory
    /// (`gate.mandatory-hosted`) — the canonical hosted remote's writes
    /// are actually enforced, not merely annotated.
    #[must_use]
    pub fn mode(&self) -> Mode {
        Mode::Mandatory
    }
}

/// The real, on-disk object store, transparently extended to also read
/// through a `pre-receive` quarantine directory when the environment names
/// one (`GIT_OBJECT_DIRECTORY`).
///
/// # Why an in-process read chain, not a written `info/alternates` file
///
/// git's own `pre-receive` quarantine does *not* write a physical
/// `info/alternates` file into the quarantine directory it hands hook
/// processes — it communicates the real odb's location purely via the
/// `GIT_ALTERNATE_OBJECT_DIRECTORIES` environment variable (confirmed
/// empirically: a quarantine directory git creates has no
/// `info/alternates` at all). `gix_odb::at`, in contrast, only ever
/// follows a physical alternates *file* — it does not consult this
/// environment variable itself. This is exactly the "which object
/// directory... is the composition root's responsibility to wire" gap
/// `ents_receive::receive`'s own doc names for the quarantine case.
///
/// The first attempt at closing this gap wrote the environment's paths
/// into the quarantine's own `info/alternates` file, reasoning that the
/// quarantine directory is discarded once the push resolves. That
/// reasoning was wrong: git's quarantine finalization *moves* the
/// quarantine directory's entire contents — including any file a hook
/// process wrote into it — onto the real object directory once the push
/// is accepted, permanently persisting a written alternates file at
/// `objects/info/alternates` whose own content names `objects` itself, a
/// self-cycle that fails every future open of the repository (observed
/// directly: a second push into the same repository failed with
/// `gix_odb`'s own "Alternates form a cycle" error, `objects` pointing at
/// itself). Nothing here writes to the object directory at all now,
/// closing that hazard structurally rather than by adding another
/// disk-state special case.
///
/// # Examples
///
/// ```
/// # let dir = tempfile::tempdir().expect("tempdir");
/// # gix::init_bare(dir.path()).expect("init"); // HostedRoot always runs against a bare repo.
/// use git_ents::root::QuarantineObjects;
///
/// // No `GIT_OBJECT_DIRECTORY` set: reads go straight to the real odb.
/// let objects = QuarantineObjects::open(dir.path()).expect("opens");
/// let missing = gix_hash::ObjectId::null(gix_hash::Kind::Sha1);
/// assert!(
///     gix_object::Find::try_find(&objects, &missing, &mut Vec::new())
///         .expect("a missing lookup is Ok(None), not an error")
///         .is_none()
/// );
/// ```
pub struct QuarantineObjects {
    /// The quarantine directory's own odb, when `GIT_OBJECT_DIRECTORY`
    /// names one distinct from the repository's real `objects/` —
    /// consulted first, so a push's own not-yet-committed objects are
    /// visible before the fallback.
    quarantine: Option<gix_odb::Handle>,
    /// The repository's real, on-disk odb — reads fall back to this, and
    /// every write always lands here (see [`gix_object::Write`]'s impl):
    /// `pre-receive` never writes objects itself
    /// (`ents_gate::verify` is read-only), so this is only exercised by
    /// `post-receive`'s write-back path, which never runs under a
    /// quarantine at all.
    real: gix_odb::Handle,
}

impl QuarantineObjects {
    /// Open the object store for the repository at `path`, chaining a
    /// `pre-receive` quarantine directory in front of the real odb when
    /// the environment names one distinct from `path`'s own `objects/`
    /// (canonicalized, since a quarantine and the real directory can
    /// otherwise compare unequal only by an unresolved symlink or a `/./`
    /// path component).
    ///
    /// # Errors
    ///
    /// [`Error::Io`] if either directory cannot be opened as an object
    /// store.
    pub fn open(path: &Path) -> Result<Self> {
        let real_dir = path.join("objects");
        let real = gix_odb::at(&real_dir).map_err(|source| Error::Io {
            path: real_dir.clone(),
            source,
        })?;
        let quarantine = match std::env::var_os("GIT_OBJECT_DIRECTORY") {
            Some(dir) => {
                let dir = PathBuf::from(dir);
                let is_real_quarantine = match (dir.canonicalize(), real_dir.canonicalize()) {
                    (Ok(q), Ok(r)) => q != r,
                    _ => dir != real_dir,
                };
                if is_real_quarantine {
                    Some(gix_odb::at(&dir).map_err(|source| Error::Io { path: dir, source })?)
                } else {
                    None
                }
            }
            None => None,
        };
        Ok(Self { quarantine, real })
    }
}

// @relation(arch.no-object-store-trait, scope=function)
impl gix_object::Find for QuarantineObjects {
    fn try_find<'a>(
        &self,
        id: &gix_hash::oid,
        buffer: &'a mut Vec<u8>,
    ) -> std::result::Result<Option<gix_object::Data<'a>>, gix_object::find::Error> {
        // The quarantine attempt reads into its own, function-local
        // buffer rather than the caller's `buffer` (whose lifetime `'a`
        // is named, not elided, so the borrow checker cannot shrink a
        // second, conditional reborrow of it to a sub-region even though
        // only one branch ever executes at runtime) — copying the found
        // bytes into `buffer` afterward keeps this the same one-call-site
        // shape `gix_odb::memory::Proxy::try_find` uses for its own
        // primary-then-fallback lookup.
        if let Some(quarantine) = &self.quarantine {
            let mut local = Vec::new();
            if let Some(found) = quarantine.try_find(id, &mut local)? {
                let kind = found.kind;
                buffer.clear();
                buffer.extend_from_slice(found.data);
                return Ok(Some(gix_object::Data {
                    kind,
                    object_hash: id.kind(),
                    data: buffer.as_slice(),
                }));
            }
        }
        self.real.try_find(id, buffer)
    }
}

impl gix_object::Write for QuarantineObjects {
    fn write_stream(
        &self,
        kind: gix_object::Kind,
        size: u64,
        from: &mut dyn std::io::Read,
    ) -> std::result::Result<gix_hash::ObjectId, gix_object::write::Error> {
        self.real.write_stream(kind, size, from)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "unit test")]

    use rstest::rstest;

    use super::*;

    /// `arch.no-hosted-branch`: the two roots are distinct types with a
    /// fixed, compile-time-chosen gate policy each — never one type
    /// branching on a runtime "am I hosted?" check. Table-driven because
    /// the spec enumerates exactly these two cases, one per root.
    #[rstest]
    #[case::local_is_advisory(true, Mode::Advisory)]
    #[case::hosted_is_mandatory(false, Mode::Mandatory)]
    // @relation(arch.no-hosted-branch, roots.config-isolation, scope=function, role=Verifies)
    fn each_root_has_one_fixed_mode(#[case] local: bool, #[case] expected: Mode) {
        let dir = tempfile::tempdir().expect("tempdir");
        let mode = if local {
            gix::init(dir.path()).expect("init");
            LocalRoot::open(dir.path()).expect("opens").mode()
        } else {
            gix::init_bare(dir.path()).expect("init bare");
            HostedRoot::open(dir.path()).expect("opens").mode()
        };
        assert_eq!(mode, expected);
    }

    /// `arch.store-composition-root`: opening either root is the *only*
    /// place a `LooseRefStore`/odb pair gets constructed — every command
    /// module ([`crate::commands`]) only ever receives an already-built
    /// root, never builds one of its own store handles.
    #[rstest]
    // @relation(arch.store-composition-root, scope=function, role=Verifies)
    fn opening_a_root_is_the_only_construction_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        gix::init(dir.path()).expect("init");
        let root = LocalRoot::open(dir.path()).expect("opens");
        // The root itself is the seam every command module is handed;
        // there is no second, parallel way to obtain a `RefStore`/odb
        // pair for this repository within this crate.
        assert_eq!(root.path, dir.path());
    }

    /// `arch.no-object-store-trait`: `QuarantineObjects` reads and writes
    /// exclusively through gitoxide's own `Find`/`Write` traits — no
    /// private object-store trait exists in this crate for it to
    /// implement instead.
    #[rstest]
    // @relation(arch.no-object-store-trait, scope=function, role=Verifies)
    fn quarantine_objects_round_trips_through_gitoxides_own_traits() {
        let dir = tempfile::tempdir().expect("tempdir");
        gix::init_bare(dir.path()).expect("init bare");
        let objects = QuarantineObjects::open(dir.path()).expect("opens");

        let oid = gix_object::Write::write(&objects, &gix_object::Tree::empty()).expect("writes");
        let mut buf = Vec::new();
        let found = gix_object::Find::try_find(&objects, &oid, &mut buf)
            .expect("reads")
            .expect("just written");
        assert_eq!(found.kind, gix_object::Kind::Tree);
    }
}
