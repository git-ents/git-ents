//! Real SSH commit signing: the production counterpart to
//! `ents_testutil::Keypair` — the same SSHSIG-over-`git`-namespace shape
//! `ents_gate::signature` verifies, but loaded from the user's own key
//! instead of a deterministic test seed.
//!
//! This is new work, not a port: `pre-redo`'s CLI shelled out to `git
//! commit -S`/`git push --signed` and let stock git invoke
//! `ssh-keygen -Y sign` itself. The redone architecture's mutation
//! frontends build and sign the commit object themselves, in-process,
//! before handing it to [`ents_receive::receive`] (`receive.unit`), so
//! `git-ents` needs its own signer rather than a subprocess shelling to
//! `git`.

use std::path::{Path, PathBuf};

use ssh_key::{HashAlg, LineEnding, PrivateKey};

use crate::error::{Error, Result};

/// The SSHSIG namespace git signs commits under — mirrors
/// `ents_testutil::keys::GIT_SIGN_NAMESPACE` and
/// `ents_gate::signature`'s verification side.
const GIT_SIGN_NAMESPACE: &str = "git";

/// A loaded SSH signing identity: the private key material plus its
/// OpenSSH public-key line, exactly the string an
/// [`ents_model::Member::key`] carries.
///
/// # Examples
///
/// ```
/// # use ssh_key::private::{Ed25519Keypair, KeypairData};
/// # let dir = tempfile::tempdir().expect("tempdir");
/// # let path = dir.path().join("id_ed25519");
/// # let pair = Ed25519Keypair::from_seed(&[7; 32]);
/// # let key = ssh_key::PrivateKey::new(KeypairData::from(pair), "test").expect("well-formed");
/// # key.write_openssh_file(&path, ssh_key::LineEnding::LF).expect("write");
/// use git_ents::sign::Signer;
///
/// let signer = Signer::load(&path).expect("loads");
/// assert!(signer.public_openssh().starts_with("ssh-ed25519 "));
///
/// let pem = signer.sign(b"payload");
/// assert!(pem.starts_with("-----BEGIN SSH SIGNATURE-----"));
/// ```
#[derive(Debug)]
pub struct Signer {
    private: PrivateKey,
}

impl Signer {
    /// Load a signing identity from an OpenSSH private key file at `path`.
    ///
    /// # Errors
    ///
    /// [`Error::BadSigningKey`] if `path` cannot be read, is not a
    /// well-formed OpenSSH private key, or is passphrase-protected — an
    /// encrypted key is a deliberate deferral (see this module's own
    /// doc): this phase supports only an unencrypted key file.
    pub fn load(path: &Path) -> Result<Self> {
        let private =
            PrivateKey::read_openssh_file(path).map_err(|source| Error::BadSigningKey {
                path: path.to_owned(),
                detail: source.to_string(),
            })?;
        if private.is_encrypted() {
            return Err(Error::BadSigningKey {
                path: path.to_owned(),
                detail: "passphrase-protected keys are not supported yet; use an unencrypted key \
                         or ssh-agent (deferred)"
                    .to_owned(),
            });
        }
        Ok(Self { private })
    }

    /// The public half in OpenSSH single-line format — what a
    /// [`ents_model::Member`]'s `key` field stores.
    #[must_use]
    pub fn public_openssh(&self) -> String {
        #[expect(
            clippy::expect_used,
            reason = "rendering an already-loaded key's own public half cannot fail; mirrors \
                      `ents_testutil::Keypair::public_openssh`'s identical, unguarded call"
        )]
        self.private
            .public_key()
            .to_openssh()
            .expect("a loaded key's public half always renders")
    }

    /// Sign `payload` in git's SSHSIG namespace, returning the armored PEM
    /// block git stores in a commit's `gpgsig` header — identical shape to
    /// `ents_testutil::Keypair::sign`.
    ///
    /// # Panics
    ///
    /// Never for a well-formed loaded key; signing an arbitrary byte
    /// payload cannot fail for the algorithms this module accepts.
    #[must_use]
    pub fn sign(&self, payload: &[u8]) -> String {
        #[expect(
            clippy::expect_used,
            reason = "signing and PEM-rendering an ed25519 signature over any byte payload is \
                      infallible; mirrors `ents_testutil::Keypair::sign`'s identical, unguarded call"
        )]
        self.private
            .sign(GIT_SIGN_NAMESPACE, HashAlg::Sha512, payload)
            .expect("signing is infallible for a loaded, unencrypted key")
            .to_pem(LineEnding::LF)
            .expect("an SSHSIG always renders as PEM")
    }
}

/// Resolve the signing key path a command should use: `--key` if given,
/// else the repository's (or global) `user.signingkey`, else the default
/// `~/.ssh/id_ed25519`.
///
/// # Errors
///
/// [`Error::NoSigningKey`] when none of the three sources resolves to a
/// path.
pub fn resolve_key_path(repo: &gix::Repository, explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path.to_owned());
    }
    if let Some(configured) = repo
        .config_snapshot()
        .string("user.signingkey")
        .map(|v| v.to_string())
    {
        return Ok(PathBuf::from(configured));
    }
    if let Some(home) = home_dir() {
        let default = home.join(".ssh").join("id_ed25519");
        if default.exists() {
            return Ok(default);
        }
    }
    Err(Error::NoSigningKey)
}

/// The current user's home directory, however the platform exposes it —
/// `$HOME` on every platform `git-ents` targets.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}
