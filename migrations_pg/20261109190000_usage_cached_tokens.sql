-- PostgreSQL twin of the sqlite usage-cached-tokens migration.
--
-- BIGINT to match prompt_tokens and completion_tokens: the shared row
-- mapper reads every integer column as i64 on both backends.
--
-- Cached token counts, as reported by upstreams that support prompt
-- caching: OpenAI chat carries them in
-- `prompt_tokens_details.cached_tokens` and
-- `completion_tokens_details.cached_tokens`, DeepSeek in
-- `prompt_cache_hit_tokens`. NULL means the upstream reported no cached
-- tokens, which is not the same as a reported zero.
ALTER TABLE request_logs ADD COLUMN cached_input_tokens BIGINT NULL;
ALTER TABLE request_logs ADD COLUMN cached_output_tokens BIGINT NULL;
