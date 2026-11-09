-- Per-key month-to-date aggregates scan `request_logs` by key and time
-- range, so the index leads with `api_key_id` and then `created_at`. The
-- existing index leads with `created_at`, which cannot serve a per-key
-- range narrow enough to matter. Additive: a plain CREATE INDEX, no table
-- rebuild.
CREATE INDEX idx_request_logs_api_key_id_created_at
    ON request_logs (api_key_id, created_at);
