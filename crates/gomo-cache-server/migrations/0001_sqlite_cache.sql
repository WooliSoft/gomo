CREATE TABLE workspaces (
  id TEXT PRIMARY KEY NOT NULL,
  slug TEXT NOT NULL UNIQUE,
  enabled INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
  shared_retention_seconds INTEGER NOT NULL DEFAULT 2592000,
  isolated_retention_seconds INTEGER NOT NULL DEFAULT 604800,
  maximum_entry_size INTEGER NOT NULL DEFAULT 10737418240,
  total_storage_quota INTEGER NOT NULL DEFAULT 1099511627776,
  reserved_bytes INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE TABLE principals (
  id TEXT PRIMARY KEY NOT NULL,
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
  kind TEXT NOT NULL CHECK (kind IN ('user', 'service', 'ci')),
  external_subject TEXT NOT NULL,
  display_name TEXT NOT NULL,
  enabled INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
  created_at INTEGER NOT NULL DEFAULT (unixepoch()),
  last_seen_at INTEGER,
  UNIQUE (workspace_id, kind, external_subject)
);

CREATE TABLE access_tokens (
  id TEXT PRIMARY KEY NOT NULL,
  public_prefix TEXT NOT NULL,
  principal_id TEXT NOT NULL REFERENCES principals(id) ON DELETE CASCADE,
  token_hash BLOB NOT NULL UNIQUE,
  capabilities TEXT NOT NULL,
  expires_at INTEGER,
  revoked_at INTEGER,
  created_at INTEGER NOT NULL DEFAULT (unixepoch()),
  last_used_at INTEGER,
  allowed_run_scope TEXT,
  source_repository TEXT,
  source_ref TEXT,
  source_commit TEXT,
  source_run TEXT
);
CREATE INDEX access_tokens_public_prefix_idx ON access_tokens (public_prefix);
CREATE INDEX access_tokens_principal_idx ON access_tokens (principal_id);

CREATE TABLE oidc_trust_rules (
  id TEXT PRIMARY KEY NOT NULL,
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
  issuer TEXT NOT NULL,
  audience TEXT NOT NULL,
  repository_owner_id TEXT NOT NULL,
  repository_id TEXT NOT NULL,
  reusable_workflow_ref TEXT,
  trusted_refs TEXT NOT NULL DEFAULT '[]',
  trusted_environments TEXT NOT NULL DEFAULT '[]',
  capabilities TEXT NOT NULL
);
CREATE UNIQUE INDEX oidc_trust_rules_identity_idx
  ON oidc_trust_rules (
    workspace_id,
    issuer,
    audience,
    repository_id,
    ifnull(reusable_workflow_ref, '')
  );

CREATE TABLE cache_scopes (
  id TEXT PRIMARY KEY NOT NULL,
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
  kind TEXT NOT NULL CHECK (kind IN ('shared', 'run', 'private')),
  external_scope_key TEXT NOT NULL,
  owner_principal_id TEXT REFERENCES principals(id) ON DELETE CASCADE,
  expires_at INTEGER,
  created_at INTEGER NOT NULL DEFAULT (unixepoch()),
  UNIQUE (workspace_id, kind, external_scope_key),
  CONSTRAINT cache_scopes_private_owner_check
    CHECK (kind <> 'private' OR owner_principal_id IS NOT NULL)
);

CREATE TABLE blobs (
  id TEXT PRIMARY KEY NOT NULL,
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
  bundle_digest TEXT NOT NULL,
  object_key TEXT NOT NULL UNIQUE,
  compressed_byte_length INTEGER NOT NULL CHECK (compressed_byte_length >= 0),
  state TEXT NOT NULL CHECK (state IN ('uploading', 'available', 'deleting', 'failed')),
  created_at INTEGER NOT NULL DEFAULT (unixepoch()),
  last_verified_at INTEGER
);
CREATE INDEX blobs_unreferenced_idx ON blobs (state, created_at);
CREATE INDEX blobs_workspace_created_idx ON blobs (workspace_id, created_at);

CREATE TABLE upload_sessions (
  id TEXT PRIMARY KEY NOT NULL,
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
  scope_id TEXT NOT NULL REFERENCES cache_scopes(id) ON DELETE CASCADE,
  principal_id TEXT NOT NULL REFERENCES principals(id) ON DELETE CASCADE,
  task_hash TEXT NOT NULL,
  object_key TEXT NOT NULL UNIQUE,
  expected_byte_length INTEGER NOT NULL,
  reserved_quota_bytes INTEGER NOT NULL,
  state TEXT NOT NULL CHECK (state IN ('uploading', 'complete', 'failed', 'expired')),
  expires_at INTEGER NOT NULL,
  created_at INTEGER NOT NULL DEFAULT (unixepoch()),
  completed_at INTEGER
);
CREATE INDEX upload_sessions_expiry_idx
  ON upload_sessions (workspace_id, state, expires_at);

CREATE TABLE cache_entries (
  id TEXT PRIMARY KEY NOT NULL,
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
  scope_id TEXT NOT NULL REFERENCES cache_scopes(id) ON DELETE CASCADE,
  protocol_version TEXT NOT NULL,
  cache_schema_version TEXT NOT NULL,
  task_hash TEXT NOT NULL,
  result_digest TEXT NOT NULL,
  blob_id TEXT NOT NULL REFERENCES blobs(id),
  producer_principal_id TEXT NOT NULL REFERENCES principals(id),
  created_at INTEGER NOT NULL DEFAULT (unixepoch()),
  last_access_at INTEGER NOT NULL DEFAULT (unixepoch()),
  source_repository TEXT,
  source_ref TEXT,
  source_commit TEXT,
  source_run TEXT,
  UNIQUE (workspace_id, scope_id, protocol_version, task_hash)
);
CREATE INDEX cache_entries_retention_idx ON cache_entries (last_access_at);
CREATE INDEX cache_entries_blob_idx ON cache_entries (blob_id);
CREATE INDEX cache_entries_scope_idx ON cache_entries (scope_id);

CREATE TABLE cache_events (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  principal_id TEXT REFERENCES principals(id),
  workspace_id TEXT REFERENCES workspaces(id),
  scope_id TEXT REFERENCES cache_scopes(id),
  operation TEXT NOT NULL,
  task_hash TEXT,
  result TEXT NOT NULL,
  byte_length INTEGER,
  request_id TEXT NOT NULL,
  network_metadata_hash TEXT,
  created_at INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE INDEX cache_events_created_idx ON cache_events (created_at);
