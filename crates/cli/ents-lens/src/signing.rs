//! The signing identity the composition root injects into the lens
//! (`lens.serve`, `roots.web-agnostic`).
//!
//! The lens writes new comments through the same signed mutation path
//! every other frontend uses (`lens.parity`), so it must be handed an
//! identity to sign with — but, exactly like `ents-web`, it resolves no
//! key itself and assumes nothing about which editor (if any) is attached.
//! [`Signing`] is a plain owned value the root builds once and moves in;
//! there is no second implementation to abstract over the way `ents-web`'s
//! hosted/local split needs, because a lens only ever serves the local
//! root (`lens.serve`), so a concrete carrier is enough and no trait is
//! introduced.

/// A closure that signs a commit's to-be-signed bytes, producing an armored
/// SSHSIG PEM block — the injected half of [`Signing`].
pub type SignFn = Box<dyn Fn(&[u8]) -> String>;

/// An owned signing identity: the commit author signature and a closure
/// that produces an SSHSIG armored block for a commit's bytes, plus the
/// public key that identifies the acting member.
///
/// Built by the composition root from the user's own key (the same
/// resolution `git ents comment` and `git ents serve` perform) and moved
/// into the [`crate::Lens`]; the lens never resolves a key path or reads
/// `user.signingkey` itself.
///
/// # Examples
///
/// ```
/// use ents_lens::Signing;
///
/// let signing = Signing::new(
///     gix::actor::Signature {
///         name: "jdc".into(),
///         email: "jdc@ents.test".into(),
///         time: gix::date::Time { seconds: 0, offset: 0 },
///     },
///     Box::new(|_payload| "-----BEGIN SSH SIGNATURE-----\n-----END SSH SIGNATURE-----\n".to_owned()),
///     "ssh-ed25519 AAAA... jdc".to_owned(),
/// );
/// assert_eq!(signing.actor().name, "jdc");
/// ```
pub struct Signing {
    actor: gix::actor::Signature,
    sign: SignFn,
    public_openssh: String,
}

impl Signing {
    /// Build a signing identity from an already-resolved key: the commit
    /// `actor` signature, a `sign` closure over the key, and the key's
    /// `public_openssh` single-line form.
    #[must_use]
    pub fn new(actor: gix::actor::Signature, sign: SignFn, public_openssh: String) -> Self {
        Self {
            actor,
            sign,
            public_openssh,
        }
    }

    /// The commit author/committer signature every comment mutation this
    /// identity signs will carry.
    #[must_use]
    pub fn actor(&self) -> gix::actor::Signature {
        self.actor.clone()
    }

    /// The public half of this identity's key, in OpenSSH single-line
    /// form — which enrolled member is acting.
    #[must_use]
    pub fn public_openssh(&self) -> &str {
        &self.public_openssh
    }

    /// Sign `payload` (a commit's to-be-signed bytes), returning the
    /// armored SSHSIG PEM block for the commit's `gpgsig` header.
    #[must_use]
    pub fn sign(&self, payload: &[u8]) -> String {
        (self.sign)(payload)
    }
}
