-- PostgreSQL twin of the sqlite channel model_prices migration.
--
-- Same additive column, same JSON TEXT shape and NULL meaning; INTEGER
-- micro-USD per 1M tokens inside the JSON, so no numeric column type is
-- involved.
ALTER TABLE channels ADD COLUMN model_prices TEXT;
