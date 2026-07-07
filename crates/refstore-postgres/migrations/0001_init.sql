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

-- Pack registry: minimal columns only. WS5 (Tigris object store) consumes
-- this to record promotion of staged packs; nothing here reads it yet.
CREATE TABLE IF NOT EXISTS git_ents_pack_registry (
    id BIGSERIAL PRIMARY KEY,
    repo_id TEXT NOT NULL,
    location_key TEXT NOT NULL,
    promoted_at TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS git_ents_pack_registry_repo_idx
    ON git_ents_pack_registry (repo_id);

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
