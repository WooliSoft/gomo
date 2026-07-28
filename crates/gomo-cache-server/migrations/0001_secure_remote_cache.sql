CREATE EXTENSION IF NOT EXISTS pgcrypto;

CREATE TYPE principal_kind AS ENUM ('user', 'service', 'ci');
CREATE TYPE scope_kind AS ENUM ('shared', 'run', 'private');
CREATE TYPE blob_state AS ENUM ('uploading', 'available', 'deleting', 'failed');
CREATE TYPE upload_state AS ENUM ('uploading', 'complete', 'failed', 'expired');

CREATE TABLE workspaces (
  id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  slug text NOT NULL UNIQUE,
  enabled boolean NOT NULL DEFAULT true,
  shared_retention_seconds bigint NOT NULL DEFAULT 2592000,
  isolated_retention_seconds bigint NOT NULL DEFAULT 604800,
  maximum_entry_size bigint NOT NULL DEFAULT 10737418240,
  total_storage_quota bigint NOT NULL DEFAULT 1099511627776,
  reserved_bytes bigint NOT NULL DEFAULT 0,
  created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE principals (
  id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
  kind principal_kind NOT NULL,
  external_subject text NOT NULL,
  display_name text NOT NULL,
  enabled boolean NOT NULL DEFAULT true,
  created_at timestamptz NOT NULL DEFAULT now(),
  last_seen_at timestamptz,
  UNIQUE (workspace_id, kind, external_subject)
);

CREATE TABLE access_tokens (
  id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  public_prefix text NOT NULL,
  principal_id uuid NOT NULL REFERENCES principals(id) ON DELETE CASCADE,
  token_hash bytea NOT NULL UNIQUE,
  capabilities text[] NOT NULL,
  expires_at timestamptz,
  revoked_at timestamptz,
  created_at timestamptz NOT NULL DEFAULT now(),
  last_used_at timestamptz
);

CREATE TABLE oidc_trust_rules (
  id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
  issuer text NOT NULL,
  audience text NOT NULL,
  repository_owner_id text NOT NULL,
  repository_id text NOT NULL,
  reusable_workflow_ref text,
  trusted_refs text[] NOT NULL DEFAULT '{}',
  trusted_environments text[] NOT NULL DEFAULT '{}',
  capabilities text[] NOT NULL,
  UNIQUE NULLS NOT DISTINCT (workspace_id, issuer, audience, repository_id, reusable_workflow_ref)
);

CREATE TABLE cache_scopes (
  id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
  kind scope_kind NOT NULL,
  external_scope_key text NOT NULL,
  owner_principal_id uuid REFERENCES principals(id) ON DELETE CASCADE,
  expires_at timestamptz,
  created_at timestamptz NOT NULL DEFAULT now(),
  UNIQUE (workspace_id, kind, external_scope_key),
  CONSTRAINT cache_scopes_private_owner_check
    CHECK (kind <> 'private' OR owner_principal_id IS NOT NULL)
);

CREATE TABLE blobs (
  id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
  bundle_digest text NOT NULL,
  object_key text NOT NULL UNIQUE,
  compressed_byte_length bigint NOT NULL CHECK (compressed_byte_length >= 0),
  state blob_state NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now(),
  last_verified_at timestamptz
);
CREATE INDEX blobs_unreferenced_idx ON blobs (state, created_at);

CREATE TABLE upload_sessions (
  id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
  scope_id uuid NOT NULL REFERENCES cache_scopes(id) ON DELETE CASCADE,
  principal_id uuid NOT NULL REFERENCES principals(id) ON DELETE CASCADE,
  task_hash text NOT NULL,
  object_key text NOT NULL UNIQUE,
  expected_byte_length bigint NOT NULL,
  reserved_quota_bytes bigint NOT NULL,
  state upload_state NOT NULL,
  expires_at timestamptz NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now(),
  completed_at timestamptz
);

CREATE TABLE cache_entries (
  id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
  scope_id uuid NOT NULL REFERENCES cache_scopes(id) ON DELETE CASCADE,
  protocol_version text NOT NULL,
  cache_schema_version text NOT NULL,
  task_hash text NOT NULL,
  result_digest text NOT NULL,
  blob_id uuid NOT NULL REFERENCES blobs(id),
  producer_principal_id uuid NOT NULL REFERENCES principals(id),
  created_at timestamptz NOT NULL DEFAULT now(),
  last_access_at timestamptz NOT NULL DEFAULT now(),
  UNIQUE (workspace_id, scope_id, protocol_version, task_hash)
);
CREATE INDEX cache_entries_retention_idx ON cache_entries (last_access_at);

CREATE TABLE blob_leases (
  blob_id uuid NOT NULL REFERENCES blobs(id) ON DELETE CASCADE,
  request_id uuid NOT NULL,
  service_instance_id text NOT NULL,
  expires_at timestamptz NOT NULL,
  PRIMARY KEY (blob_id, request_id)
);

CREATE TABLE cache_events (
  id bigserial PRIMARY KEY,
  principal_id uuid REFERENCES principals(id),
  workspace_id uuid REFERENCES workspaces(id),
  scope_id uuid REFERENCES cache_scopes(id),
  operation text NOT NULL,
  task_hash text,
  result text NOT NULL,
  byte_length bigint,
  request_id uuid NOT NULL,
  network_metadata_hash text,
  created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX cache_events_created_idx ON cache_events (created_at);
