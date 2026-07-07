//! [`PostgresRefStore::log_corpus_entry`]/[`PostgresRefStore::corpus_log`]:
//! durable storage for `git_protocol::CorpusEntry` (`docs/scale-out.adoc`,
//! WS0's "op replay corpus"), over `git_ents_corpus_log`. Kept in this crate
//! rather than `git-protocol` itself for the same reason `pack_registry.rs`
//! and `queue.rs` are here: `git-protocol` never needs a `tokio-postgres`
//! dependency of its own.

use git_backend::{Error, Result};
use git_protocol::CorpusEntry;
use git_protocol::types::AppliedRefEdit;
use gix_hash::ObjectId;

use crate::{PostgresRefStore, pg_err};

/// Serialize `edits` as one `name\told\tnew` line each (`-` standing in for
/// a missing old/new oid), so the whole batch fits in a single `TEXT`
/// column without pulling in a serialization dependency this crate doesn't
/// already have.
fn encode_ref_edits(edits: &[AppliedRefEdit]) -> String {
    edits
        .iter()
        .map(|edit| {
            let old = edit
                .old
                .map_or_else(|| "-".to_owned(), |oid| oid.to_hex().to_string());
            let new = edit
                .new
                .map_or_else(|| "-".to_owned(), |oid| oid.to_hex().to_string());
            format!("{}\t{old}\t{new}", edit.name)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parse [`encode_ref_edits`]'s format back into [`AppliedRefEdit`]s. A line
/// that cannot be parsed (malformed hex, wrong field count) is skipped
/// rather than failing the whole read — a corpus reader degrades to a
/// shorter replay rather than an unusable one.
fn decode_ref_edits(text: &str) -> Vec<AppliedRefEdit> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.splitn(3, '\t');
            let name = fields.next()?;
            let old = fields.next()?;
            let new = fields.next()?;
            Some(AppliedRefEdit {
                name: git_backend::RefName::new(name),
                old: parse_optional_oid(old),
                new: parse_optional_oid(new),
            })
        })
        .collect()
}

fn parse_optional_oid(hex: &str) -> Option<ObjectId> {
    if hex == "-" {
        None
    } else {
        ObjectId::from_hex(hex.as_bytes()).ok()
    }
}

impl PostgresRefStore {
    /// Durably append one accepted push's [`CorpusEntry`] to this store's
    /// repo-scoped corpus log.
    ///
    /// # Errors
    ///
    /// Returns [`Error::RefStore`] if the insert fails.
    pub fn log_corpus_entry(&self, entry: &CorpusEntry) -> Result<()> {
        let push_cert_oid = entry.push_cert_oid.map(|oid| oid.to_hex().to_string());
        let ref_edits = encode_ref_edits(&entry.ref_edits);
        self.runtime
            .block_on(async {
                let client = self.client.lock().await;
                client
                    .execute(
                        "INSERT INTO git_ents_corpus_log
                             (repo_id, push_cert_oid, ref_edits, pack)
                         VALUES ($1, $2, $3, $4)",
                        &[&self.repo_id, &push_cert_oid, &ref_edits, &entry.pack],
                    )
                    .await
            })
            .map_err(pg_err)
            .map(|_rows_affected| ())
    }

    /// This store's logged corpus entries, oldest first — the order a
    /// replay must apply them in.
    ///
    /// # Errors
    ///
    /// Returns [`Error::RefStore`] if the query fails, or a stored
    /// `push_cert_oid` is not valid hex (a corrupted row, never written by
    /// [`Self::log_corpus_entry`]).
    pub fn corpus_log(&self) -> Result<Vec<CorpusEntry>> {
        let rows = self
            .runtime
            .block_on(async {
                let client = self.client.lock().await;
                client
                    .query(
                        "SELECT push_cert_oid, ref_edits, pack
                         FROM git_ents_corpus_log
                         WHERE repo_id = $1 ORDER BY id ASC",
                        &[&self.repo_id],
                    )
                    .await
            })
            .map_err(pg_err)?;

        rows.into_iter()
            .map(|row| {
                let push_cert_oid: Option<String> = row.try_get(0).map_err(pg_err)?;
                let ref_edits: String = row.try_get(1).map_err(pg_err)?;
                let pack: Vec<u8> = row.try_get(2).map_err(pg_err)?;
                let push_cert_oid = push_cert_oid
                    .map(|hex| ObjectId::from_hex(hex.as_bytes()))
                    .transpose()
                    .map_err(|error| Error::RefStore(error.to_string()))?;
                Ok(CorpusEntry {
                    push_cert_oid,
                    ref_edits: decode_ref_edits(&ref_edits),
                    pack,
                })
            })
            .collect()
    }
}
