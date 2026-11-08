-- PostgreSQL twin of the sqlite nullable-ids migration.
--
-- Request rejections are logged into `request_logs` too, and a rejection
-- whose caller never resolved (unknown key 401) references neither an
-- api_keys row nor a channels row, so both columns become nullable.

ALTER TABLE request_logs ALTER COLUMN api_key_id DROP NOT NULL;
ALTER TABLE request_logs ALTER COLUMN channel_id DROP NOT NULL;
