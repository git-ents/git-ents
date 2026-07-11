//! Commit signature extraction and offline verification
//! (`gate.tip-signed`, `gate.signature-artifact`).
//!
//! The signature is read from the commit object's `gpgsig` header — a
//! data artifact that replicates with the repository — and verified in
//! pure Rust against a member's stored OpenSSH public key. Nothing here
//! reads a push certificate, the environment, or any transport state:
//! give this module the same bytes in any clone and it returns the same
//! answer (`gate.signature-artifact`).

use ssh_key::{PublicKey, SshSig};

/// The SSHSIG namespace git signs commits under.
const GIT_NAMESPACE: &str = "git";

/// Split a raw commit object into its signed payload and its detached
/// signature: the payload is the commit serialization with the `gpgsig`
/// header removed, exactly the bytes git signs.
///
/// Returns `None` when the commit carries no `gpgsig` header — an
/// unsigned commit.
pub(crate) fn split_signed(raw: &[u8]) -> Option<(Vec<u8>, String)> {
    // The header section ends at the first blank line; gpgsig is a
    // header whose continuation lines start with a single space.
    let header_end = raw
        .windows(2)
        .position(|w| w == b"\n\n")
        .map_or(raw.len(), |i| i.saturating_add(1));

    let mut sig_start = None;
    let mut sig_end = None;
    let mut line_start = 0usize;
    while line_start < header_end {
        let rest = raw.get(line_start..header_end)?;
        let line_len = rest
            .iter()
            .position(|&b| b == b'\n')
            .map_or(rest.len(), |i| i.saturating_add(1));
        let line = rest.get(..line_len)?;
        if sig_start.is_none() {
            if line.starts_with(b"gpgsig ") {
                sig_start = Some(line_start);
                sig_end = Some(line_start.saturating_add(line_len));
            }
        } else if sig_end == Some(line_start) && line.starts_with(b" ") {
            sig_end = Some(line_start.saturating_add(line_len));
        }
        line_start = line_start.saturating_add(line_len);
    }

    let (start, end) = (sig_start?, sig_end?);
    let mut payload = Vec::with_capacity(raw.len().saturating_sub(end.saturating_sub(start)));
    payload.extend_from_slice(raw.get(..start)?);
    payload.extend_from_slice(raw.get(end..)?);

    let header = raw.get(start..end)?;
    let text = std::str::from_utf8(header).ok()?;
    let mut sig = String::new();
    for (i, line) in text.lines().enumerate() {
        let value = if i == 0 {
            line.strip_prefix("gpgsig ")?
        } else {
            line.strip_prefix(' ')?
        };
        sig.push_str(value);
        sig.push('\n');
    }
    Some((payload, sig))
}

/// Whether `signature` (an armored SSHSIG) over `payload` verifies
/// against `key` (an OpenSSH single-line public key, as stored on a
/// [`ents_model::Member`]).
///
/// Any malformed key, malformed signature, wrong namespace, or failed
/// cryptographic check is `false` — the gate treats them all as "not
/// signed by this member", and the caller renders which member set was
/// consulted.
pub(crate) fn verifies(key: &str, payload: &[u8], signature: &str) -> bool {
    let Ok(key) = PublicKey::from_openssh(key) else {
        return false;
    };
    let Ok(sig) = SshSig::from_pem(signature) else {
        return false;
    };
    key.verify(GIT_NAMESPACE, payload, &sig).is_ok()
}

#[cfg(test)]
mod tests {
    #![expect(clippy::expect_used, reason = "unit test")]

    use rstest::rstest;

    use super::*;

    /// A hand-written commit shape: gpgsig between committer and an
    /// extra header, with two continuation lines (each continuation
    /// line starts with one space, as git writes multi-line headers).
    const RAW: &[u8] = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\nauthor A <a@a> 100 +0000\ncommitter A <a@a> 100 +0000\ngpgsig -----BEGIN SSH SIGNATURE-----\n QUJD\n -----END SSH SIGNATURE-----\nother value\n\nmessage body\n";

    #[rstest]
    // @relation(gate.signature-artifact, scope=function, role=Verifies)
    fn split_removes_exactly_the_gpgsig_header() {
        let (payload, sig) = split_signed(RAW).expect("signed");
        assert_eq!(
            sig,
            "-----BEGIN SSH SIGNATURE-----\nQUJD\n-----END SSH SIGNATURE-----\n"
        );
        let expected: &[u8] = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\nauthor A <a@a> 100 +0000\ncommitter A <a@a> 100 +0000\nother value\n\nmessage body\n";
        assert_eq!(payload, expected);
    }

    #[rstest]
    fn unsigned_commit_splits_to_none() {
        let raw = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\nauthor A <a@a> 100 +0000\ncommitter A <a@a> 100 +0000\n\ngpgsig in the message is not a header\n";
        assert!(split_signed(raw).is_none());
    }
}
