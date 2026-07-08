-- Schema for `refstore-postgres` (`docs/scale-out.adoc`, "RefStore" / WS4).
-- Applied idempotently: every statement is guarded so running this file
-- against an already-migrated database is a no-op.

-- One row per ref. The primary key is also the prefix-iteration index: a
-- `text_pattern_ops` index makes `LIKE 'prefix%'` scans (used by
-- `iter_prefix`) index-backed regardless of the database's default locale,
-- since that opclass compares raw bytes rather than collated text.
CREATE TABLE IF NOT EXISTS git_ents_refs (
    repo_id TEXT NOT NULL,
    name TEXT NOT NULL,
    oid TEXT NOT NULL,
    PRIMARY KEY (repo_id, name)
);

CREATE INDEX IF NOT EXISTS git_ents_refs_prefix_idx
    ON git_ents_refs (repo_id, name text_pattern_ops);

-- Append-only reflog: one row per applied `RefEdit`, written in the same SQL
-- transaction as the ref mutation it records.
CREATE TABLE IF NOT EXISTS git_ents_reflog (
    id BIGSERIAL PRIMARY KEY,
    repo_id TEXT NOT NULL,
    name TEXT NOT NULL,
    old_oid TEXT,
    new_oid TEXT,
    message TEXT NOT NULL,
    recorded_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS git_ents_reflog_lookup_idx
    ON git_ents_reflog (repo_id, name, id DESC);

-- Pack registry: WS5 (Tigris object store) records promoted packs here, and
-- `odb_tigris::OdbTigris::read`/`contains` (via `PostgresRefStore`'s
-- `odb_tigris::registry::PackRegistry` impl, see `pack_registry.rs`) consult
-- only this table — never a bucket listing (`docs/scale-out.adoc`,
-- "Reachability").
CREATE TABLE IF NOT EXISTS git_ents_pack_registry (
    id BIGSERIAL PRIMARY KEY,
    repo_id TEXT NOT NULL,
    location_key TEXT NOT NULL,
    promoted_at TIMESTAMPTZ
);

-- `location_key` predates the `PackRegistry` trait (WS5) and is unused by
-- it; dropping its `NOT NULL` rather than removing it keeps this migration
-- idempotent against a database that already has rows from before this
-- change, without inventing a fake value for a column nothing here writes
-- anymore.
ALTER TABLE git_ents_pack_registry ALTER COLUMN location_key DROP NOT NULL;

ALTER TABLE git_ents_pack_registry ADD COLUMN IF NOT EXISTS pack_id TEXT;
ALTER TABLE git_ents_pack_registry ADD COLUMN IF NOT EXISTS pack_key TEXT;
ALTER TABLE git_ents_pack_registry ADD COLUMN IF NOT EXISTS idx_key TEXT;
ALTER TABLE git_ents_pack_registry ADD COLUMN IF NOT EXISTS object_count BIGINT;

CREATE INDEX IF NOT EXISTS git_ents_pack_registry_repo_idx
    ON git_ents_pack_registry (repo_id);

CREATE UNIQUE INDEX IF NOT EXISTS git_ents_pack_registry_repo_pack_idx
    ON git_ents_pack_registry (repo_id, pack_id);

-- Small-object tier (WS5, `docs/scale-out.adoc`'s `odb-tiered` row): blobs
-- and trees under `odb_tiered::OdbTiered`'s size threshold, staged then
-- promoted like any other object storage tier (correctness rules 1 and 2
-- apply here too). `stage_id` identifies an in-flight batch; `promoted`
-- flips to true (and `stage_id` clears) in the single `UPDATE` that is this
-- tier's whole promotion transaction (see `small_tier.rs`).
CREATE TABLE IF NOT EXISTS git_ents_small_objects (
    repo_id TEXT NOT NULL,
    oid TEXT NOT NULL,
    kind TEXT NOT NULL,
    bytes BYTEA NOT NULL,
    stage_id TEXT,
    promoted BOOLEAN NOT NULL DEFAULT FALSE,
    PRIMARY KEY (repo_id, oid)
);

CREATE INDEX IF NOT EXISTS git_ents_small_objects_stage_idx
    ON git_ents_small_objects (stage_id)
    WHERE stage_id IS NOT NULL;

-- Effect queue: the at-least-once source of truth `watch`'s NOTIFY hint
-- points consumers back at (`docs/scale-out.adoc`, "RefStore": "the effect
-- queue table ... is the source of truth").
CREATE TABLE IF NOT EXISTS git_ents_effect_queue (
    id BIGSERIAL PRIMARY KEY,
    repo_id TEXT NOT NULL,
    payload TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'enqueued'
        CHECK (state IN ('enqueued', 'claimed', 'done')),
    claimed_by TEXT,
    enqueued_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    claimed_at TIMESTAMPTZ,
    done_at TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS git_ents_effect_queue_state_idx
    ON git_ents_effect_queue (repo_id, state, id);

-- Op records index: one row per accepted push, pointing at the op record
-- object (the push-cert-plus-outcome artifact `git-protocol` builds via
-- `attestation::build_op_record`) by OID.
CREATE TABLE IF NOT EXISTS git_ents_op_records (
    id BIGSERIAL PRIMARY KEY,
    repo_id TEXT NOT NULL,
    op_oid TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS git_ents_op_records_repo_idx
    ON git_ents_op_records (repo_id, created_at DESC);

-- Op-replay corpus (WS0, `docs/scale-out.adoc`'s "op replay corpus" /
-- WS2's conformance seed corpus): one row per accepted push through the
-- hydration write path, carrying enough to replay it against any
-- RefStore+ObjectStore pair — see `git_protocol::corpus::CorpusEntry`,
-- which this table's rows serialize. `ref_edits` is one `name\told\tnew`
-- line per edit (`-` for a missing old/new); `pack` is the exact bytes
-- staged for the push (may be empty, e.g. a pure ref deletion).
CREATE TABLE IF NOT EXISTS git_ents_corpus_log (
    id BIGSERIAL PRIMARY KEY,
    repo_id TEXT NOT NULL,
    push_cert_oid TEXT,
    ref_edits TEXT NOT NULL,
    pack BYTEA NOT NULL,
    recorded_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS git_ents_corpus_log_repo_idx
    ON git_ents_corpus_log (repo_id, id);

-- Reachability artifacts (WS6, `docs/scale-out.adoc`'s "Reachability"
-- section): the commit-graph and reachable-set accelerators
-- `gix-reachability`'s maintenance effect generates, tracked here rather
-- than by bucket listing, same as packs above. One row per `(repo_id,
-- kind)` — regenerating overwrites the existing row instead of
-- accumulating snapshots, so `kind` alone (not a generated id) is enough to
-- look one up.
CREATE TABLE IF NOT EXISTS git_ents_reachability_artifacts (
    repo_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    key TEXT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (repo_id, kind)
);
