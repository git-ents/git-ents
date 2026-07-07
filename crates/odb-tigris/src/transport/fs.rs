//! [`FsTransport`]: a [`BlobTransport`] over a local directory, standing in
//! for the bucket in tests and conformance — no network, so the suite
//! (`crates/odb-tigris/tests/conformance.rs`) runs anywhere
//! (`docs/scale-out.adoc`, WS5, "Conformance").

use std::ops::Range;
use std::path::PathBuf;

use git_backend::Result;

use super::{BlobTransport, transport_err};

/// A [`BlobTransport`] backed by plain files under a root directory. Keys
/// (e.g. `"repo/live/abc.pack"`) map onto `root/repo/live/abc.pack`,
/// creating parent directories as needed on `put`.
pub struct FsTransport {
    root: PathBuf,
}

impl FsTransport {
    /// Store blobs under `root`, creating it if it does not exist.
    ///
    /// # Errors
    ///
    /// Returns an error if `root` cannot be created.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    fn path_for(&self, key: &str) -> PathBuf {
        self.root.join(key)
    }
}

impl BlobTransport for FsTransport {
    fn put(&self, key: &str, bytes: Vec<u8>) -> Result<()> {
        let path = self.path_for(key);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, bytes)?;
        Ok(())
    }

    fn get(&self, key: &str) -> Result<Vec<u8>> {
        Ok(std::fs::read(self.path_for(key))?)
    }

    fn get_range(&self, key: &str, range: Range<u64>) -> Result<Vec<u8>> {
        // No partial-file read syscall is worth the complexity here: this
        // transport exists for tests, not for latency. It still honors the
        // clamping contract so growth-loop callers see the same behavior a
        // real ranged GET would.
        let data = self.get(key)?;
        let len = data.len() as u64;
        let start = range.start.min(len);
        let end = range.end.min(len);
        Ok(if start >= end {
            Vec::new()
        } else {
            data.get(usize_of(start)..usize_of(end))
                .map(<[u8]>::to_vec)
                .unwrap_or_default()
        })
    }

    fn exists(&self, key: &str) -> Result<bool> {
        Ok(self.path_for(key).is_file())
    }

    fn delete(&self, key: &str) -> Result<()> {
        let path = self.path_for(key);
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn copy(&self, from: &str, to: &str) -> Result<()> {
        let to_path = self.path_for(to);
        if let Some(parent) = to_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(self.path_for(from), &to_path)
            .map_err(|error| transport_err(&format!("copy {from} -> {to}"), error))?;
        Ok(())
    }
}

/// Fallible-looking but total for the lengths this module deals with:
/// callers only ever pass values already clamped to a file's actual byte
/// length, which never approaches `usize::MAX`.
fn usize_of(n: u64) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}
