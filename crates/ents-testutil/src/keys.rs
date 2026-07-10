//! Deterministic SSH signing keys for fixtures.

use ssh_key::private::{Ed25519Keypair, KeypairData};
use ssh_key::{HashAlg, LineEnding, PrivateKey};

/// The SSHSIG namespace git uses when signing commits with an SSH key.
pub const GIT_SIGN_NAMESPACE: &str = "git";

/// A deterministic ed25519 keypair for signing fixture commits.
///
/// Deterministic (seeded) rather than random so property tests and
/// build-the-fixture-twice "clone" tests reproduce byte-identical objects.
///
/// # Examples
///
/// ```
/// use ents_testutil::Keypair;
///
/// let a = Keypair::from_seed(1);
/// let b = Keypair::from_seed(1);
/// assert_eq!(a.public_openssh(), b.public_openssh());
/// assert_ne!(a.public_openssh(), Keypair::from_seed(2).public_openssh());
/// ```
#[derive(Debug)]
pub struct Keypair {
    private: PrivateKey,
}

impl Keypair {
    /// Derive a keypair from a one-byte seed (repeated to fill the ed25519
    /// seed), so tests can name keys `1`, `2`, ... and always get the same
    /// key back.
    #[must_use]
    pub fn from_seed(seed: u8) -> Self {
        let pair = Ed25519Keypair::from_seed(&[seed; 32]);
        let private = PrivateKey::new(KeypairData::from(pair), "ents-testutil")
            .expect("a well-formed ed25519 keypair is always accepted");
        Self { private }
    }

    /// The public half in OpenSSH single-line format — exactly what a
    /// [`ents_model::Member`]'s `key` field carries.
    ///
    /// # Examples
    ///
    /// ```
    /// use ents_testutil::Keypair;
    ///
    /// let key = Keypair::from_seed(1).public_openssh();
    /// assert!(key.starts_with("ssh-ed25519 "));
    /// ```
    #[must_use]
    pub fn public_openssh(&self) -> String {
        self.private
            .public_key()
            .to_openssh()
            .expect("an ed25519 public key always renders")
    }

    /// Sign `payload` in git's SSHSIG namespace, returning the armored
    /// signature block git would store in a commit's `gpgsig` header.
    ///
    /// # Examples
    ///
    /// ```
    /// use ents_testutil::Keypair;
    ///
    /// let pem = Keypair::from_seed(1).sign(b"payload");
    /// assert!(pem.starts_with("-----BEGIN SSH SIGNATURE-----"));
    /// ```
    #[must_use]
    pub fn sign(&self, payload: &[u8]) -> String {
        self.private
            .sign(GIT_SIGN_NAMESPACE, HashAlg::Sha512, payload)
            .expect("ed25519 signing is infallible for any payload")
            .to_pem(LineEnding::LF)
            .expect("an SSHSIG always renders as PEM")
    }
}
