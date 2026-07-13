//! `git ents serve`: reuse [`LocalRoot`]'s existing wiring and add only
//! the `ents-web` HTTP frontend, bound to loopback (`roots.local`).
//!
//! `roots.local` is explicit that this command MUST reuse the local
//! root's own seams rather than construct a second one, and MUST NOT
//! expose git's smart-HTTP transport in any form. This module upholds
//! both: [`build_state`] is handed an already-open [`LocalRoot`] (never
//! opens its own), and adds nothing but `ents_web::router()`'s own route
//! table -- which carries no `/info/refs` or `git-upload-pack` surface at
//! all (see `ents-web`'s own test coverage for that half).
//!
//! # Signing identity (`roots.web-signing`)
//!
//! `LocalIdentity` is the one place this crate bridges its own
//! [`Signer`] to [`ents_web::identity::SigningIdentity`]: the local root
//! signs every web edit with the user's own member key, resolved exactly
//! as every other mutation command resolves it (`--key`, else
//! `user.signingkey`, else the default `~/.ssh/id_ed25519`) — no
//! server-key indirection exists anywhere in this module, which is
//! exactly what keeps `roots.web-signing`'s hosted-only indirection from
//! leaking into the local root. `LocalIdentity::label` additionally
//! resolves the signer's own enrolled member (reusing
//! `crate::commands::members::find_by_key`, the same key-match loop
//! `git ents members check` runs), so the web shell's identity chip shows
//! a username instead of [`actor`]'s fixed `"git-ents"` commit-author
//! wordmark.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use ents_web::identity::SigningIdentity;
use ents_web::state::AppState;

use super::actor;
use crate::error::{Error, Result};
use crate::root::LocalRoot;
use crate::sign::Signer;

/// Bridges [`Signer`] to [`SigningIdentity`]: the local root's half of
/// `roots.web-signing`'s indirection (the user's own key, captured once
/// at `serve` startup rather than re-resolved per request).
// @relation(roots.web-signing, scope=file)
struct LocalIdentity {
    signer: Signer,
    actor: gix::actor::Signature,
    /// The web shell's identity-chip label (see [`SigningIdentity::label`]'s
    /// own doc): the signer's enrolled member username when one matches,
    /// its short key fingerprint otherwise — resolved once in
    /// [`build_state`], not per request.
    label: String,
}

impl SigningIdentity for LocalIdentity {
    fn actor(&self) -> gix::actor::Signature {
        self.actor.clone()
    }

    fn sign(&self, payload: &[u8]) -> String {
        self.signer.sign(payload)
    }

    fn public_openssh(&self) -> String {
        self.signer.public_openssh()
    }

    fn label(&self) -> String {
        self.label.clone()
    }
}

/// The loopback address `git ents serve` binds -- `roots.local` forbids
/// this command from exposing anything but loopback, so there is no
/// `--host` flag anywhere in [`crate::cli`] to override it.
// @relation(roots.local, scope=function)
fn loopback_addr(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

/// Build the [`AppState`] `git ents serve` runs, from an already-open
/// [`LocalRoot`] -- the one seam-wiring step `roots.composition` allows,
/// and the only place this command touches `root`'s fields at all.
///
/// Split out from [`run`] so tests can drive the resulting state through
/// `ents_web::router()` directly (via `tower::ServiceExt::oneshot`, per
/// `roots.web-agnostic`) without binding a socket or blocking.
///
/// # Errors
///
/// Propagates a signing-key resolution failure ([`crate::sign::Signer`]).
// @relation(roots.local, roots.composition, scope=function)
pub fn build_state(
    root: LocalRoot,
    key: Option<PathBuf>,
) -> Result<Arc<AppState<crate::root::Objects>>> {
    let signer = super::signer(&root, key)?;
    let pubkey = signer.public_openssh();
    // The identity chip's label (`roots.web-signing`): reuse the same
    // key-match loop `git ents members check` runs (`find_by_key`) rather
    // than re-scanning `refs/meta/member/*` by hand, falling back to the
    // signer's own short fingerprint when no enrolled member's key matches
    // (an unenrolled local key, still allowed to browse and sign).
    let label = super::members::find_by_key(&root, &pubkey)?
        .map(|(username, _state)| username)
        .unwrap_or_else(|| super::short_fingerprint(&signer));
    let identity = LocalIdentity {
        actor: actor(&signer),
        label,
        signer,
    };
    let mode = root.mode();
    let LocalRoot {
        path,
        refs,
        objects,
        events,
        executor: _,
    } = root;
    Ok(Arc::new(AppState::new(
        Box::new(refs),
        objects,
        Box::new(events),
        mode,
        Box::new(identity),
        path,
    )))
}

/// Run `git ents serve`: bind loopback and block, serving the web UI
/// until the process is killed.
///
/// # Errors
///
/// Propagates [`build_state`]'s own errors, or an [`Error::Io`] binding
/// the loopback socket or constructing the async runtime.
// @relation(roots.local, scope=function)
pub fn run(
    root: LocalRoot,
    port: Option<u16>,
    key: Option<PathBuf>,
    mut report: impl std::io::Write,
) -> Result<()> {
    let state = build_state(root, key)?;
    let addr = loopback_addr(port.unwrap_or(4880));

    let runtime = tokio::runtime::Runtime::new().map_err(|source| Error::Io {
        path: PathBuf::from("<tokio runtime>"),
        source,
    })?;
    runtime.block_on(async move {
        let listener = ents_web::bind(addr).await.map_err(|source| Error::Io {
            path: PathBuf::from(addr.to_string()),
            source,
        })?;
        let bound = listener.local_addr().map_err(|source| Error::Io {
            path: PathBuf::from(addr.to_string()),
            source,
        })?;
        let _ = writeln!(report, "listening on http://{bound}");
        ents_web::serve_on(listener, state)
            .await
            .map_err(|source| Error::Io {
                path: PathBuf::from(addr.to_string()),
                source,
            })
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "unit test")]

    use rstest::rstest;

    use super::*;

    #[rstest]
    // @relation(roots.local, scope=function, role=Verifies)
    fn serve_only_ever_binds_loopback() {
        assert_eq!(loopback_addr(4880).ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(loopback_addr(0).ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
    }
}
