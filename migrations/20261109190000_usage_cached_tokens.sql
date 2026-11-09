-- Cached token counts, as reported by upstreams that support prompt
-- caching: OpenAI chat carries them in
-- `prompt_tokens_details.cached_tokens` and
-- `completion_tokens_details.cached_tokens`, DeepSeek in
-- `prompt_cache_hit_tokens`. NULL means the upstream reported no cached
-- tokens, which is not the same as a reported zero.
ALTER TABLE request_logs ADD COLUMN cached_input_tokens INTEGER NULL;
ALTER TABLE request_logs ADD COLUMN cached_output_tokens INTEGER NULL;
