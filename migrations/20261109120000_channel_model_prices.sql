-- Per-channel model prices, stored as JSON TEXT and read back by the
-- channel mapper. NULL means "no prices configured for this channel"; the
-- object shape is model name -> input, output, cached_input, cached_output
-- in integer micro-USD per 1M tokens.
ALTER TABLE channels ADD COLUMN model_prices TEXT;
