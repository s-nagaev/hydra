CREATE TABLE channels (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    base_url TEXT NOT NULL,
    api_key TEXT NOT NULL,
    allowed_models TEXT,
    enabled INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL
);

CREATE TABLE api_keys (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    key_hash TEXT NOT NULL UNIQUE,
    display_prefix TEXT NOT NULL,
    name TEXT,
    channel_id INTEGER NOT NULL REFERENCES channels(id),
    enabled INTEGER NOT NULL DEFAULT 1,
    deleted_at TEXT,
    created_at TEXT NOT NULL
);

CREATE TABLE request_logs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    api_key_id INTEGER NOT NULL REFERENCES api_keys(id) ON DELETE RESTRICT,
    channel_id INTEGER NOT NULL REFERENCES channels(id),
    model TEXT,
    status INTEGER,
    prompt_tokens INTEGER,
    completion_tokens INTEGER,
    duration_ms INTEGER,
    request_body TEXT,
    response_body TEXT,
    error TEXT,
    created_at TEXT NOT NULL
);

CREATE INDEX idx_request_logs_created_at_api_key_id_channel_id
    ON request_logs (created_at, api_key_id, channel_id);
