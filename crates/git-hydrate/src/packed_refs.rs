//! The read-path's other half (`docs/scale-out.adoc`, WS0's read path):
//! regenerate `packed-refs` from the ref store on every `info/refs`
//! request, bounding advertisement staleness to one request's worth.

use std::path::Path;

use git_backend::{RefName, RefStore, Result};

/// Rewrite `repo_path`'s `packed-refs` from one
/// [`RefStore::iter_prefix`]`("refs/")` scan over `refs`, atomically (a
/// temp file, then a rename) so a concurrent `git` reader never observes a
/// half-written file.
///
/// No peeled (`^{}`) entries are emitted for annotated tags — this rewrite
/// intentionally does not claim the `fully-peeled` trait git's
/// `packed-refs` format supports, so a reader that needs a tag's peeled
/// target still resolves it correctly by opening the tag object itself,
/// just without the fast path a fully-peeled file would offer. Correctness
/// over an optimization this backend does not need yet.
///
/// # Errors
///
/// Returns an error if `refs` cannot be scanned or the file cannot be
/// written.
pub fn regenerate(repo_path: &Path, refs: &dyn RefStore) -> Result<()> {
    let mut entries: Vec<(String, String)> = refs
        .iter_prefix(&RefName::new("refs/"))?
        .map(|entry| entry.map(|(name, oid)| (name.as_str().to_owned(), oid.to_hex().to_string())))
        .collect::<Result<_>>()?;
    entries.sort();

    let mut body = String::from("# pack-refs with: sorted\n");
    for (name, oid) in entries {
        body.push_str(&oid);
        body.push(' ');
        body.push_str(&name);
        body.push('\n');
    }

    let tmp_path = repo_path.join("packed-refs.tmp");
    std::fs::write(&tmp_path, body)?;
    std::fs::rename(&tmp_path, repo_path.join("packed-refs"))?;
    Ok(())
}
