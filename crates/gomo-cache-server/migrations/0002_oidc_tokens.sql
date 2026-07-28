-- no-transaction

ALTER TABLE access_tokens
  ADD COLUMN IF NOT EXISTS allowed_run_scope text;

CREATE INDEX CONCURRENTLY IF NOT EXISTS access_tokens_public_prefix_idx
  ON access_tokens (public_prefix);
