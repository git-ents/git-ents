//! The signing-identity seam (`roots.web-agnostic`, `roots.web-signing`):
//! the one new trait this crate introduces, because gitoxide and the
//! kernel are both silent on "who signs a web-originated commit" -- exactly
//! the carve-out `arch.no-object-store-trait` reserves for "the pluggable
//! ref store, server-side receive framing, and reachability artifacts...
//! and new seams where upstream is silent."
//!
//! Every page that proposes a mutation is handed a
//! `&dyn SigningIdentity` through [`crate::state::AppState`]; nothing in
//! [`crate::pages`] ever loads a key, resolves `user.signingkey`, or knows
//! whether the identity behind it belongs to the local operator or a
//! hosted server's own worker account. That is the whole point
//! (`roots.web-signing`): a hosted deployment's composition root wires an
//! identity backed by the server's own enrolled member key, a local
//! deployment's wires one backed by the user's own key
//! (`git_ents::sign::Signer`, injected by `git-ents`'s own `serve`
//! command) -- both satisfy this same trait, and no branch anywhere in
//! this crate asks which one it was handed.

/// Everything a page handler needs to sign a mutation commit on behalf of
/// the current request, injected by the composition root
/// (`roots.web-agnostic`).
///
/// # Examples
///
/// A fixture identity, standing in for either a local user's key or a
/// hosted server's worker key -- [`crate::pages`] cannot tell which from
/// this trait alone, which is exactly `roots.web-signing`'s requirement.
///
/// ```
/// use ents_web::identity::SigningIdentity;
///
/// struct Fixed;
/// impl SigningIdentity for Fixed {
///     fn actor(&self) -> gix::actor::Signature {
///         gix::actor::Signature {
///             name: "fixture".into(),
///             email: "fixture@ents.test".into(),
///             time: gix::date::Time { seconds: 0, offset: 0 },
///         }
///     }
///     fn sign(&self, _payload: &[u8]) -> String {
///         "-----BEGIN SSH SIGNATURE-----\n-----END SSH SIGNATURE-----\n".to_owned()
///     }
///     fn public_openssh(&self) -> String {
///         "ssh-ed25519 AAAA... fixture".to_owned()
///     }
/// }
///
/// let identity: Box<dyn SigningIdentity> = Box::new(Fixed);
/// assert_eq!(identity.actor().name, "fixture");
/// // `label` defaults to `actor().name` when a composition root has no
/// // better identifier (a resolved member's own username, for instance).
/// assert_eq!(identity.label(), "fixture");
/// ```
// @relation(roots.web-signing, roots.web-agnostic, scope=file)
pub trait SigningIdentity: Send + Sync {
    /// The commit author/committer signature every mutation this identity
    /// signs carries.
    fn actor(&self) -> gix::actor::Signature;

    /// Sign `payload` (a commit's to-be-signed bytes), returning the
    /// armored SSHSIG PEM block for the commit's `gpgsig` header.
    fn sign(&self, payload: &[u8]) -> String;

    /// The public half of this identity's key, in OpenSSH single-line
    /// format -- used to resolve which enrolled [`ents_model::Member`] is
    /// acting, exactly as `git ents account create` resolves its own
    /// signer's member when `--member` is omitted.
    fn public_openssh(&self) -> String;

    /// This identity's display label for `crate::pages::layout`'s
    /// `.id-chip` (`roots.web-signing`) -- the one place this crate names
    /// "who is acting" for a human reader, as opposed to [`Self::actor`]'s
    /// commit-authorship signature.
    ///
    /// Defaults to [`Self::actor`]'s own author name: good enough when a
    /// composition root has nothing better to show. `git-ents`'s own
    /// `LocalIdentity` overrides this with the enrolled member's username
    /// resolved from the signer's public key (falling back to a short key
    /// fingerprint when no member matches), since `actor().name` there is
    /// a fixed wordmark ("git-ents"), not a signer identity -- showing it
    /// in the chip would just duplicate the site logo next to it.
    fn label(&self) -> String {
        self.actor().name.to_string()
    }
}

/// Build the [`ents_receive::Identity`] every mutation page hands to
/// `propose_entity`/`propose_delete`.
///
/// This is a macro, not a function, deliberately: `ents_receive::Identity`
/// borrows its `sign` closure (`sign: &'a dyn Fn(&[u8]) -> String`), so the
/// closure literal must live in the caller's own stack frame -- a helper
/// function that built and returned an `Identity` would return a
/// reference to a temporary dropped at that function's end. Every page in
/// [`crate::pages`] expands this at its own call site instead, exactly the
/// shape `git_ents::commands::comment::add` and its siblings already use.
#[macro_export]
macro_rules! receive_identity {
    ($identity:expr) => {
        $crate::receive_identity!($identity, None)
    };
    // The attributed form (`receive.attributed-author`,
    // `roots.web-signing`): `$author` is an
    // `Option<gix::actor::Signature>` naming the signed-in member --
    // `pages::member_author(&session)` at every mutation call site -- so
    // hosted history reads "member via the web" while the committer and
    // signature stay the injected identity. Under a `Trusted` policy the
    // session never holds a member, the option is `None`, and the commit
    // is byte-identical to the single-argument form's.
    ($identity:expr, $author:expr) => {
        ents_receive::Identity {
            actor: $identity.actor(),
            author: $author,
            sign: &|payload| $identity.sign(payload),
        }
    };
}
