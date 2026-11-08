-- Request rejections are logged into `request_logs` too, and a rejection
-- whose caller never resolved (unknown key 401) references neither an
-- api_keys row nor a channels row, so both columns become nullable.
-- SQLite cannot relax a column constraint in place: the table is rebuilt
-- and copied row by row, then the index is recreated.
CREATE TABLE request_logs_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    api_key_id INTEGER REFERENCES api_keys(id) ON DELETE RESTRICT,
    channel_id INTEGER REFERENCES channels(id),
    model TEXT,
    status INTEGER,
    prompt_tokens INTEGER,
    completion_tokens INTEGER,
    duration_ms INTEGER,
    request_body TEXT,
    response_body TEXT,
    error TEXT,
    client_ip TEXT,
    created_at TEXT NOT NULL
);

INSERT INTO request_logs_new (id, api_key_id, channel_id, model, status,
    prompt_tokens, completion_tokens, duration_ms, request_body,
    response_body, error, client_ip, created_at)
SELECT id, api_key_id, channel_id, model, status,
    prompt_tokens, completion_tokens, duration_ms, request_body,
    response_body, error, client_ip, created_at
FROM request_logs;

DROP TABLE request_logs;
ALTER TABLE request_logs_new RENAME TO request_logs;

CREATE INDEX idx_request_logs_created_at_api_key_id_channel_id
    ON request_logs (created_at, api_key_id, channel_id);
