# Gomo cache server

The cache server keeps publication and authorization in a local SQLite database
and stores immutable bundle bytes in an S3-compatible bucket. It is intended to
run as a single server process. Clients receive only an HTTP bearer token; do
not distribute the S3 key.

This server is intended for infra deployments, not local devenv. Local Garage
from `devenv up -d` is for apps and tests; point a deployed cache server at its
own bucket and credentials.

Bind `GOMO_CACHE_LISTEN` to a reachable address (or terminate TLS on a reverse
proxy in front of a loopback listener). Set `GOMO_CACHE_S3_ENDPOINT` to the
deployed Garage/S3 endpoint; `http://127.0.0.1:3900` is only valid when Garage
runs on the same host as the cache server.

```sh
export GOMO_CACHE_DATABASE_URL=sqlite:///var/lib/gomo-cache/gomo-cache.db
export GOMO_CACHE_WORKSPACE=wooli
export GOMO_CACHE_LISTEN=0.0.0.0:7788
export GOMO_CACHE_READ_TOKEN="a-generated-read-only-token"
export GOMO_CACHE_WRITE_TOKEN="a-different-generated-read-write-token"
export GOMO_CACHE_S3_BUCKET=gomo-cache
export GOMO_CACHE_S3_ENDPOINT=https://garage.example.internal
export GOMO_CACHE_S3_REGION=garage
export GOMO_CACHE_S3_FORCE_PATH_STYLE=true
export GOMO_CACHE_S3_ACCESS_KEY_ID="the-runtime-key-id"
export GOMO_CACHE_S3_SECRET_ACCESS_KEY="the-runtime-secret"

cargo run -p gomo-cache-server -- migrate
cargo run -p gomo-cache-server -- doctor
cargo run -p gomo-cache-server -- serve
```

The database defaults to `sqlite://gomo-cache.db` in the current directory.
Set an absolute URL in deployments and persist the database file together with
its `-wal` and `-shm` files. SQLite uses exclusive locking, so only one server or
administration command can access the database at a time. Stop `serve` before
running `migrate`, `doctor`, or a one-off `gc`. `serve` embeds and applies its
migrations at startup and runs garbage collection in-process every hour. Set
`GOMO_CACHE_GC_INTERVAL_SECONDS` and `GOMO_CACHE_GC_BATCH_SIZE` to change that
schedule.

Never commit Garage credentials or bearer tokens. The runtime Garage key should
only read and write this bucket.

The read token receives only `cache:shared:read`. The write token receives
`cache:shared:read` and `cache:shared:write`; reserve it for protected CI. The
write token also authorizes the metrics endpoint. Run-scoped clients set
`GOMO_REMOTE_CACHE_RUN_ID`.

For GitHub OIDC, insert a trust rule after migration. Immutable repository
owner/repository IDs are required:

```sql
INSERT INTO oidc_trust_rules (
  id, workspace_id, issuer, audience, repository_owner_id, repository_id,
  reusable_workflow_ref, trusted_refs, trusted_environments, capabilities
)
SELECT lower(printf(
    '%s-%s-%s-%s-%s',
    hex(randomblob(4)), hex(randomblob(2)), hex(randomblob(2)),
    hex(randomblob(2)), hex(randomblob(6))
  )), id,
  'https://token.actions.githubusercontent.com',
  'https://cache.example.internal',
  '1234567',
  '7654321',
  '.github/workflows/ci.yml@refs/heads/main',
  '["refs/heads/main"]',
  '[]',
  '["cache:shared:read","cache:shared:write","cache:run:read","cache:run:write"]'
FROM workspaces WHERE slug = 'wooli';
```

The service caches GitHub signing keys, validates RS256 signature, issuer,
expiration, audience, immutable repository IDs, and optional reusable-workflow
identity, then issues a hashed token lasting 15 minutes by default. Trusted
refs/environments receive the rule's capabilities. Other matched contexts
receive shared read plus read/write access only to
`run:<repository-id>:<run-id>:<run-attempt>`. Set
`GOMO_CACHE_OIDC_JWKS_URL` only for a controlled test issuer.

Published-object retention must be SQLite-driven. Do not attach a blind
expiration policy to `objects/`; Garage has no object versioning or object lock
to recover accidentally expired objects. Lifecycle rules may abort incomplete
multipart uploads.
