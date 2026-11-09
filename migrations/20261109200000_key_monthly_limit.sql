-- Optional monthly spending limit per client key, in integer micro-USD
-- (1 USD = 1_000_000). NULL means unlimited, which is the state every
-- existing row moves into. Additive column, no table rebuild: the shape of
-- the stored limit never changes, only its value.
ALTER TABLE api_keys ADD COLUMN monthly_limit_micro_usd INTEGER NULL;
