ALTER TABLE access_tokens
  ADD COLUMN source_repository text;
ALTER TABLE access_tokens
  ADD COLUMN source_ref text;
ALTER TABLE access_tokens
  ADD COLUMN source_commit text;
ALTER TABLE access_tokens
  ADD COLUMN source_run text;

ALTER TABLE cache_entries
  ADD COLUMN source_repository text;
ALTER TABLE cache_entries
  ADD COLUMN source_ref text;
ALTER TABLE cache_entries
  ADD COLUMN source_commit text;
ALTER TABLE cache_entries
  ADD COLUMN source_run text;
