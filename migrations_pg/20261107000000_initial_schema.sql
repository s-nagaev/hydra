-- PostgreSQL twin of the sqlite initial schema.
--
-- Identical columns and constraints, PostgreSQL dialect: BIGSERIAL keys,
-- BIGINT for the sqlite INTEGER columns the code reads as i64, BOOLEAN for
-- the enabled flags. Timestamps stay TEXT in the canonical
-- `YYYY-MM-DD HH:MM:SS` UTC format (never timestamptz), so the admin log
-- filters keep comparing strings.

CREATE TABLE channels (
    id BIGSERIAL PRIMARY KEY,
    name TEXT NOT NULL,
    base_url TEXT NOT NULL,
    api_key TEXT NOT NULL,
    allowed_models TEXT,
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TEXT NOT NULL
);

CREATE TABLE api_keys (
    id BIGSERIAL PRIMARY KEY,
    key_hash TEXT NOT NULL UNIQUE,
    display_prefix TEXT NOT NULL,
    name TEXT,
    channel_id BIGINT NOT NULL REFERENCES channels(id),
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    deleted_at TEXT,
    created_at TEXT NOT NULL
);

CREATE TABLE request_logs (
    id BIGSERIAL PRIMARY KEY,
    api_key_id BIGINT NOT NULL REFERENCES api_keys(id) ON DELETE RESTRICT,
    channel_id BIGINT NOT NULL REFERENCES channels(id),
    model TEXT,
    status BIGINT,
    prompt_tokens BIGINT,
    completion_tokens BIGINT,
    duration_ms BIGINT,
    request_body TEXT,
    response_body TEXT,
    error TEXT,
    created_at TEXT NOT NULL
);

CREATE INDEX idx_request_logs_created_at_api_key_id_channel_id
    ON request_logs (created_at, api_key_id, channel_id);
