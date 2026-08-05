use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::io::Read;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use aws_config::BehaviorVersion;
use aws_sdk_s3::Client as S3Client;
use aws_sdk_s3::config::{Credentials, Region};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
use aws_smithy_types::checksum_config::{RequestChecksumCalculation, ResponseChecksumValidation};
use axum::body::{Body, BodyDataStream};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, head, post};
use axum::{Json, Router};
use clap::{Parser, Subcommand};
use futures::StreamExt;
use gomo_cache_protocol::{
    BUNDLE_MEDIA_TYPE, CACHE_SCHEMA_VERSION, Capabilities, ErrorResponse,
    GithubOidcExchangeRequest, GithubOidcExchangeResponse, HEADER_BUNDLE_DIGEST,
    HEADER_BUNDLE_LENGTH, HEADER_CACHE_SCHEMA, HEADER_CREATED_AT, HEADER_PROTOCOL_VERSION,
    HEADER_RESULT_DIGEST, HEADER_SCOPE, HEADER_WORKSPACE, PROTOCOL_VERSION,
};
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteLockingMode, SqlitePoolOptions};
use sqlx::{Row, Sqlite, SqlitePool};
use subtle::ConstantTimeEq;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::RwLock;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::trace::TraceLayer;
use tracing::{error, info};
use uuid::Uuid;

const DEFAULT_DATABASE_URL: &str = "sqlite://gomo-cache.db";

#[derive(Parser)]
#[command(name = "gomo-cache-server", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Serve,
    Migrate,
    Doctor,
    /// Evict expired entries and unreferenced objects.
    Gc {
        #[arg(long, default_value_t = 100)]
        batch_size: i64,
    },
}

#[derive(Clone)]
struct Config {
    database_url: String,
    listen: SocketAddr,
    workspace: String,
    bucket: String,
    object_prefix: String,
    endpoint: Option<String>,
    region: String,
    access_key_id: String,
    secret_access_key: String,
    session_token: Option<String>,
    force_path_style: bool,
    token: String,
    capabilities: BTreeSet<String>,
    max_entry_size_bytes: u64,
    allowed_run_id: Option<String>,
    oidc_issuer: String,
    oidc_jwks_url: String,
    oidc_token_ttl_seconds: i64,
}

impl Config {
    fn load() -> Result<Self> {
        let listen = env::var("GOMO_CACHE_LISTEN")
            .unwrap_or_else(|_| "127.0.0.1:7788".to_string())
            .parse()
            .context("GOMO_CACHE_LISTEN must be a socket address")?;
        let capabilities = env::var("GOMO_CACHE_CAPABILITIES")
            .unwrap_or_else(|_| {
                [
                    "cache:shared:read",
                    "cache:shared:write",
                    "cache:run:read",
                    "cache:run:write",
                    "cache:private:read",
                    "cache:private:write",
                ]
                .join(",")
            })
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .collect();
        Ok(Self {
            database_url: env::var("GOMO_CACHE_DATABASE_URL")
                .unwrap_or_else(|_| DEFAULT_DATABASE_URL.to_string()),
            listen,
            workspace: required("GOMO_CACHE_WORKSPACE")?,
            bucket: required("GOMO_CACHE_S3_BUCKET")?,
            object_prefix: env::var("GOMO_CACHE_S3_PREFIX").unwrap_or_default(),
            endpoint: env::var("GOMO_CACHE_S3_ENDPOINT").ok(),
            region: env::var("GOMO_CACHE_S3_REGION").unwrap_or_else(|_| "garage".to_string()),
            access_key_id: required("GOMO_CACHE_S3_ACCESS_KEY_ID")?,
            secret_access_key: required("GOMO_CACHE_S3_SECRET_ACCESS_KEY")?,
            session_token: env::var("GOMO_CACHE_S3_SESSION_TOKEN").ok(),
            force_path_style: env::var("GOMO_CACHE_S3_FORCE_PATH_STYLE")
                .map(|value| value != "false")
                .unwrap_or(true),
            token: required("GOMO_CACHE_TOKEN")?,
            capabilities,
            max_entry_size_bytes: env::var("GOMO_CACHE_MAX_ENTRY_SIZE_BYTES")
                .unwrap_or_else(|_| "10737418240".to_string())
                .parse()
                .context("GOMO_CACHE_MAX_ENTRY_SIZE_BYTES must be an integer")?,
            allowed_run_id: env::var("GOMO_CACHE_ALLOWED_RUN_ID").ok(),
            oidc_issuer: env::var("GOMO_CACHE_OIDC_ISSUER")
                .unwrap_or_else(|_| "https://token.actions.githubusercontent.com".to_string()),
            oidc_jwks_url: env::var("GOMO_CACHE_OIDC_JWKS_URL").unwrap_or_else(|_| {
                "https://token.actions.githubusercontent.com/.well-known/jwks".to_string()
            }),
            oidc_token_ttl_seconds: env::var("GOMO_CACHE_OIDC_TOKEN_TTL_SECONDS")
                .unwrap_or_else(|_| "900".to_string())
                .parse()
                .context("GOMO_CACHE_OIDC_TOKEN_TTL_SECONDS must be an integer")?,
        })
    }
}

fn required(name: &str) -> Result<String> {
    env::var(name).with_context(|| format!("{name} is required"))
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn format_unix_timestamp(seconds: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(seconds)
        .map(|value| {
            value
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_else(|_| seconds.to_string())
        })
        .unwrap_or_else(|_| seconds.to_string())
}

fn encode_string_list(values: &[String]) -> Result<String> {
    serde_json::to_string(values).context("failed to encode string list as JSON")
}

fn decode_string_list(value: &str) -> Result<Vec<String>> {
    serde_json::from_str(value).context("failed to decode string list JSON")
}

fn uuid_text(id: Uuid) -> String {
    id.to_string()
}

fn parse_uuid(value: &str) -> Result<Uuid> {
    Uuid::parse_str(value).with_context(|| format!("invalid UUID text: {value}"))
}

async fn connect_pool(database_url: &str) -> Result<SqlitePool> {
    let options = SqliteConnectOptions::from_str(database_url)
        .with_context(|| format!("invalid GOMO_CACHE_DATABASE_URL: {database_url}"))?
        .create_if_missing(true)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(5))
        .locking_mode(SqliteLockingMode::Exclusive)
        .journal_mode(SqliteJournalMode::Wal);
    SqlitePoolOptions::new()
        .min_connections(1)
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(options)
        .await
        .context("failed to connect to SQLite")
}

async fn run_migrations(pool: &SqlitePool) -> Result<()> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    sqlx::migrate::Migrator::new(path.as_path())
        .await?
        .run(pool)
        .await?;
    Ok(())
}

#[derive(Clone)]
struct AppState {
    pool: SqlitePool,
    s3: S3Client,
    config: Config,
    workspace_id: Uuid,
    metrics: Arc<Metrics>,
    oidc: Arc<OidcState>,
}

struct OidcState {
    client: reqwest::Client,
    jwks: RwLock<Option<(Instant, JwkSet)>>,
}

impl Default for OidcState {
    fn default() -> Self {
        Self {
            client: reqwest::Client::new(),
            jwks: RwLock::new(None),
        }
    }
}

#[derive(Clone)]
struct AuthContext {
    principal_id: Uuid,
    capabilities: BTreeSet<String>,
    allowed_run_scope: Option<String>,
    source_repository: Option<String>,
    source_ref: Option<String>,
    source_commit: Option<String>,
    source_run: Option<String>,
}

#[derive(Default)]
struct Metrics {
    hits: AtomicU64,
    misses: AtomicU64,
    upload_bytes: AtomicU64,
    download_bytes: AtomicU64,
    conflicts: AtomicU64,
    digest_failures: AtomicU64,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "gomo_cache_server=info,tower_http=info".into()),
        )
        .json()
        .init();
    let cli = Cli::parse();
    let config = Config::load()?;
    let pool = connect_pool(&config.database_url).await?;
    match cli.command {
        Command::Migrate => {
            run_migrations(&pool).await?;
            info!("database migrations complete");
        }
        Command::Doctor => {
            sqlx::query("SELECT 1").execute(&pool).await?;
            let s3 = s3_client(&config).await;
            s3.head_bucket()
                .bucket(&config.bucket)
                .send()
                .await
                .context("failed to access cache bucket")?;
            info!("SQLite and object storage are ready");
        }
        Command::Serve => serve(config, pool).await?,
        Command::Gc { batch_size } => {
            let state = administration_state(config, pool).await?;
            let removed = garbage_collect(&state, batch_size).await?;
            info!(removed, "garbage collection complete");
        }
    }
    Ok(())
}

async fn administration_state(config: Config, pool: SqlitePool) -> Result<AppState> {
    let workspace_id = sqlx::query_scalar::<_, String>("SELECT id FROM workspaces WHERE slug = $1")
        .bind(&config.workspace)
        .fetch_one(&pool)
        .await?;
    Ok(AppState {
        s3: s3_client(&config).await,
        config,
        pool,
        workspace_id: parse_uuid(&workspace_id)?,
        metrics: Arc::new(Metrics::default()),
        oidc: Arc::new(OidcState::default()),
    })
}

async fn garbage_collect(state: &AppState, batch_size: i64) -> Result<u64> {
    let batch_size = batch_size.max(1);
    let now = unix_now();
    sqlx::query(
        "DELETE FROM access_tokens
         WHERE (expires_at IS NOT NULL AND expires_at <= $1)
            OR (revoked_at IS NOT NULL AND revoked_at < $2)",
    )
    .bind(now)
    .bind(now - 7 * 24 * 3600)
    .execute(&state.pool)
    .await?;
    let expired_objects = expire_upload_sessions(state, batch_size).await?;
    for object_key in expired_objects {
        if let Err(error) = state
            .s3
            .delete_object()
            .bucket(&state.config.bucket)
            .key(&object_key)
            .send()
            .await
        {
            error!(%error, %object_key, "failed to remove expired upload object");
        }
    }

    let mut blob_ids = sqlx::query_scalar::<_, String>(
        "DELETE FROM cache_entries
         WHERE id IN (
           SELECT e.id
           FROM cache_entries e
           JOIN cache_scopes s ON s.id = e.scope_id
           JOIN workspaces w ON w.id = e.workspace_id
           WHERE e.workspace_id = $1
             AND e.last_access_at < $2 -
               CASE WHEN s.kind = 'shared'
                 THEN w.shared_retention_seconds ELSE w.isolated_retention_seconds END
           ORDER BY e.last_access_at
           LIMIT $3
         )
         RETURNING blob_id",
    )
    .bind(uuid_text(state.workspace_id))
    .bind(now)
    .bind(batch_size)
    .fetch_all(&state.pool)
    .await?;
    let orphan_ids = sqlx::query_scalar::<_, String>(
        "SELECT b.id
         FROM blobs b
         WHERE b.workspace_id = $1
           AND b.created_at < $2
           AND NOT EXISTS (SELECT 1 FROM cache_entries e WHERE e.blob_id = b.id)
         ORDER BY b.created_at
         LIMIT $3",
    )
    .bind(uuid_text(state.workspace_id))
    .bind(now - 3600)
    .bind(batch_size)
    .fetch_all(&state.pool)
    .await?;
    blob_ids.extend(orphan_ids);
    blob_ids.sort_unstable();
    blob_ids.dedup();
    let mut removed = 0;
    for blob_id in blob_ids {
        let row = sqlx::query(
            "SELECT object_key FROM blobs b
             WHERE b.id = $1
               AND NOT EXISTS (SELECT 1 FROM cache_entries e WHERE e.blob_id = b.id)",
        )
        .bind(&blob_id)
        .fetch_optional(&state.pool)
        .await?;
        let Some(row) = row else {
            continue;
        };
        let object_key: String = row.get("object_key");
        sqlx::query("UPDATE blobs SET state = 'deleting' WHERE id = $1")
            .bind(&blob_id)
            .execute(&state.pool)
            .await?;
        // S3 I/O is outside any DB transaction so the single SQLite connection
        // stays available to other requests.
        if let Err(error) = state
            .s3
            .delete_object()
            .bucket(&state.config.bucket)
            .key(&object_key)
            .send()
            .await
        {
            error!(%error, %blob_id, %object_key, "failed to remove unreferenced object");
            continue;
        }
        sqlx::query("DELETE FROM blobs WHERE id = $1")
            .bind(&blob_id)
            .execute(&state.pool)
            .await?;
        removed += 1;
    }
    sqlx::query(
        "DELETE FROM cache_scopes
         WHERE workspace_id = $1 AND expires_at IS NOT NULL AND expires_at <= $2
           AND NOT EXISTS (SELECT 1 FROM cache_entries e WHERE e.scope_id = cache_scopes.id)",
    )
    .bind(uuid_text(state.workspace_id))
    .bind(now)
    .execute(&state.pool)
    .await?;
    Ok(removed)
}

async fn expire_upload_sessions(state: &AppState, batch_size: i64) -> Result<Vec<String>> {
    let mut tx = state.pool.begin().await?;
    let now = unix_now();
    let rows = sqlx::query(
        "SELECT id, object_key, reserved_quota_bytes
         FROM upload_sessions
         WHERE workspace_id = $1 AND state = 'uploading' AND expires_at <= $2
         ORDER BY expires_at
         LIMIT $3",
    )
    .bind(uuid_text(state.workspace_id))
    .bind(now)
    .bind(batch_size)
    .fetch_all(&mut *tx)
    .await?;
    if rows.is_empty() {
        tx.commit().await?;
        return Ok(Vec::new());
    }
    let ids = rows
        .iter()
        .map(|row| row.get::<String, _>("id"))
        .collect::<Vec<_>>();
    let reserved = rows
        .iter()
        .map(|row| row.get::<i64, _>("reserved_quota_bytes"))
        .sum::<i64>();
    let object_keys = rows
        .iter()
        .map(|row| row.get::<String, _>("object_key"))
        .collect::<Vec<_>>();
    let ids_json = serde_json::to_string(&ids).context("failed to encode upload session ids")?;
    let updated = sqlx::query(
        "UPDATE upload_sessions
         SET state = 'expired', completed_at = $1, reserved_quota_bytes = 0
         WHERE id IN (SELECT value FROM json_each($2))
           AND state = 'uploading' AND expires_at <= $3",
    )
    .bind(now)
    .bind(ids_json)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    if updated.rows_affected() != ids.len() as u64 {
        bail!("upload sessions changed while expiring them");
    }
    sqlx::query(
        "UPDATE workspaces
         SET reserved_bytes = max(0, reserved_bytes - $2)
         WHERE id = $1",
    )
    .bind(uuid_text(state.workspace_id))
    .bind(reserved)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(object_keys)
}

async fn s3_client(config: &Config) -> S3Client {
    let credentials = Credentials::new(
        &config.access_key_id,
        &config.secret_access_key,
        config.session_token.clone(),
        None,
        "gomo-cache-server",
    );
    let mut loader = aws_config::defaults(BehaviorVersion::latest())
        .region(Region::new(config.region.clone()))
        // S3-compatible stores do not uniformly support the optional trailer
        // checksums enabled by recent AWS SDK defaults.
        .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
        .response_checksum_validation(ResponseChecksumValidation::WhenRequired)
        .credentials_provider(credentials);
    if let Some(endpoint) = &config.endpoint {
        loader = loader.endpoint_url(endpoint);
    }
    let shared = loader.load().await;
    let s3_config = aws_sdk_s3::config::Builder::from(&shared)
        .force_path_style(config.force_path_style)
        .build();
    S3Client::from_conf(s3_config)
}

async fn upload_object(
    state: &AppState,
    object_key: &str,
    path: &std::path::Path,
    byte_len: u64,
) -> Result<()> {
    const MULTIPART_THRESHOLD: u64 = 16 * 1024 * 1024;
    const MIN_PART_SIZE: u64 = 8 * 1024 * 1024;
    if byte_len < MULTIPART_THRESHOLD {
        state
            .s3
            .put_object()
            .bucket(&state.config.bucket)
            .key(object_key)
            .body(ByteStream::from_path(path).await?)
            .send()
            .await
            .map_err(|error| anyhow::anyhow!("object upload failed: {error:?}"))?;
        return Ok(());
    }

    let created = state
        .s3
        .create_multipart_upload()
        .bucket(&state.config.bucket)
        .key(object_key)
        .send()
        .await?;
    let upload_id = created
        .upload_id()
        .context("object storage omitted the multipart upload id")?
        .to_string();
    let upload_result = async {
        let part_size = MIN_PART_SIZE.max(byte_len.div_ceil(10_000));
        let part_size = usize::try_from(part_size).context("multipart part size is too large")?;
        let mut file = tokio::fs::File::open(path).await?;
        let mut buffer = vec![0_u8; part_size];
        let mut completed = Vec::new();
        let mut part_number = 1_i32;
        loop {
            let mut filled = 0;
            while filled < buffer.len() {
                let read = file.read(&mut buffer[filled..]).await?;
                if read == 0 {
                    break;
                }
                filled += read;
            }
            if filled == 0 {
                break;
            }
            let uploaded = state
                .s3
                .upload_part()
                .bucket(&state.config.bucket)
                .key(object_key)
                .upload_id(&upload_id)
                .part_number(part_number)
                .content_length(filled as i64)
                .body(ByteStream::from(buffer[..filled].to_vec()))
                .send()
                .await?;
            completed.push(
                CompletedPart::builder()
                    .part_number(part_number)
                    .set_e_tag(uploaded.e_tag().map(str::to_string))
                    .build(),
            );
            part_number += 1;
        }
        let multipart = CompletedMultipartUpload::builder()
            .set_parts(Some(completed))
            .build();
        state
            .s3
            .complete_multipart_upload()
            .bucket(&state.config.bucket)
            .key(object_key)
            .upload_id(&upload_id)
            .multipart_upload(multipart)
            .send()
            .await?;
        Result::<()>::Ok(())
    }
    .await;
    if upload_result.is_err() {
        let _ = state
            .s3
            .abort_multipart_upload()
            .bucket(&state.config.bucket)
            .key(object_key)
            .upload_id(&upload_id)
            .send()
            .await;
    }
    upload_result
}

async fn serve(config: Config, pool: SqlitePool) -> Result<()> {
    run_migrations(&pool).await?;
    let workspace_id = Uuid::new_v4();
    let workspace_id_text: String = sqlx::query_scalar(
        "INSERT INTO workspaces (id, slug, maximum_entry_size) VALUES ($1, $2, $3)
         ON CONFLICT (slug) DO UPDATE
         SET enabled = 1, maximum_entry_size = excluded.maximum_entry_size
         RETURNING id",
    )
    .bind(uuid_text(workspace_id))
    .bind(&config.workspace)
    .bind(config.max_entry_size_bytes as i64)
    .fetch_one(&pool)
    .await?;
    let workspace_id = parse_uuid(&workspace_id_text)?;
    let principal_id = Uuid::new_v4();
    let principal_id_text: String = sqlx::query_scalar(
        "INSERT INTO principals (id, workspace_id, kind, external_subject, display_name)
         VALUES ($1, $2, 'service', 'bootstrap-static-token', 'static cache token')
         ON CONFLICT (workspace_id, kind, external_subject)
         DO UPDATE SET enabled = 1, last_seen_at = $3
         RETURNING id",
    )
    .bind(uuid_text(principal_id))
    .bind(uuid_text(workspace_id))
    .bind(unix_now())
    .fetch_one(&pool)
    .await?;
    let principal_id = parse_uuid(&principal_id_text)?;
    let token_hash = *blake3::hash(config.token.as_bytes()).as_bytes();
    let public_prefix = config.token.chars().take(8).collect::<String>();
    let capability_values = config.capabilities.iter().cloned().collect::<Vec<_>>();
    let capabilities_json = encode_string_list(&capability_values)?;
    let token_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO access_tokens
           (id, public_prefix, principal_id, token_hash, capabilities, allowed_run_scope)
         VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT (token_hash) DO UPDATE
         SET revoked_at = NULL, capabilities = excluded.capabilities,
             allowed_run_scope = excluded.allowed_run_scope",
    )
    .bind(uuid_text(token_id))
    .bind(public_prefix)
    .bind(uuid_text(principal_id))
    .bind(token_hash.as_slice())
    .bind(capabilities_json)
    .bind(config.allowed_run_id.as_deref())
    .execute(&pool)
    .await?;
    let state = Arc::new(AppState {
        s3: s3_client(&config).await,
        pool,
        config: config.clone(),
        workspace_id,
        metrics: Arc::new(Metrics::default()),
        oidc: Arc::new(OidcState::default()),
    });
    let app = Router::new()
        .route("/health/live", get(|| async { StatusCode::OK }))
        .route("/health/ready", get(ready))
        .route("/metrics", get(metrics))
        .route("/v1/capabilities", get(capabilities))
        .route("/v1/auth/github", post(exchange_github_oidc))
        .route(
            "/v1/cache/{task_hash}",
            head(head_cache).get(get_cache).put(put_cache),
        )
        .route(
            "/v1/cache/{task_hash}/corruption-reports",
            post(report_corruption),
        )
        .with_state(state)
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
        .layer(TraceLayer::new_for_http());
    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    info!(listen = %config.listen, "Gomo cache server listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

async fn ready(State(state): State<Arc<AppState>>) -> Response {
    if let Err(error) = sqlx::query("SELECT 1").execute(&state.pool).await {
        error!(%error, "database readiness check failed");
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    if let Err(error) = state
        .s3
        .head_bucket()
        .bucket(&state.config.bucket)
        .send()
        .await
    {
        error!(%error, "object storage readiness check failed");
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    StatusCode::OK.into_response()
}

async fn metrics(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !authorized_metrics_token(&state, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let metrics = &state.metrics;
    format!(
        "gomo_cache_hits_total {}\n\
gomo_cache_misses_total {}\n\
gomo_cache_upload_bytes_total {}\n\
gomo_cache_download_bytes_total {}\n\
gomo_cache_conflicts_total {}\n\
gomo_cache_digest_failures_total {}\n",
        metrics.hits.load(Ordering::Relaxed),
        metrics.misses.load(Ordering::Relaxed),
        metrics.upload_bytes.load(Ordering::Relaxed),
        metrics.download_bytes.load(Ordering::Relaxed),
        metrics.conflicts.load(Ordering::Relaxed),
        metrics.digest_failures.load(Ordering::Relaxed),
    )
    .into_response()
}

fn authorized_metrics_token(state: &AppState, headers: &HeaderMap) -> bool {
    let Some(token) = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
    else {
        return false;
    };
    let presented = blake3::hash(token.as_bytes());
    let expected = blake3::hash(state.config.token.as_bytes());
    bool::from(presented.as_bytes().ct_eq(expected.as_bytes()))
}

async fn capabilities(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let (_, auth) = match authorize(&state, &headers, None, false).await {
        Ok(auth) => auth,
        Err(response) => return response,
    };
    Json(Capabilities {
        protocol_versions: vec![PROTOCOL_VERSION.to_string()],
        cache_schema_versions: vec![CACHE_SCHEMA_VERSION.to_string()],
        max_entry_size_bytes: state.config.max_entry_size_bytes,
        capabilities: auth.capabilities.into_iter().collect(),
    })
    .into_response()
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Audience {
    One(String),
    Many(Vec<String>),
}

impl Audience {
    fn contains(&self, expected: &str) -> bool {
        match self {
            Self::One(value) => value == expected,
            Self::Many(values) => values.iter().any(|value| value == expected),
        }
    }
}

#[derive(Debug, Deserialize)]
struct GithubClaims {
    iss: String,
    sub: String,
    aud: Audience,
    exp: i64,
    repository_id: String,
    repository_owner_id: String,
    repository: String,
    #[serde(default)]
    r#ref: Option<String>,
    #[serde(default)]
    environment: Option<String>,
    #[serde(default)]
    job_workflow_ref: Option<String>,
    run_id: String,
    run_attempt: String,
    sha: String,
}

async fn exchange_github_oidc(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<GithubOidcExchangeRequest>,
) -> Response {
    if header_string(&headers, HEADER_WORKSPACE).as_deref() != Some(&state.config.workspace)
        || header_string(&headers, HEADER_PROTOCOL_VERSION).as_deref() != Some(PROTOCOL_VERSION)
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let claims = match validate_github_jwt(&state, &request.jwt).await {
        Ok(claims) => claims,
        Err(error) => {
            error!(%error, "GitHub OIDC validation failed");
            return api_error(
                StatusCode::UNAUTHORIZED,
                "invalid_oidc_token",
                "GitHub OIDC token validation failed",
            );
        }
    };
    let rules = match sqlx::query(
        "SELECT audience, reusable_workflow_ref, trusted_refs,
                trusted_environments, capabilities
         FROM oidc_trust_rules
         WHERE workspace_id = $1 AND issuer = $2
           AND repository_owner_id = $3 AND repository_id = $4",
    )
    .bind(uuid_text(state.workspace_id))
    .bind(&claims.iss)
    .bind(&claims.repository_owner_id)
    .bind(&claims.repository_id)
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows,
        Err(error) => return internal_error(error.into()),
    };
    let matched = rules.into_iter().find(|row| {
        let audience: String = row.get("audience");
        let workflow: Option<String> = row.get("reusable_workflow_ref");
        claims.aud.contains(&audience)
            && workflow
                .as_deref()
                .is_none_or(|expected| claims.job_workflow_ref.as_deref() == Some(expected))
    });
    let Some(rule) = matched else {
        return api_error(
            StatusCode::FORBIDDEN,
            "oidc_trust_denied",
            "no OIDC trust rule matches this repository and workflow",
        );
    };
    let trusted_refs = match decode_string_list(&rule.get::<String, _>("trusted_refs")) {
        Ok(values) => values,
        Err(error) => return internal_error(error),
    };
    let trusted_environments =
        match decode_string_list(&rule.get::<String, _>("trusted_environments")) {
            Ok(values) => values,
            Err(error) => return internal_error(error),
        };
    let trusted = claims
        .r#ref
        .as_ref()
        .is_some_and(|value| trusted_refs.contains(value))
        || claims
            .environment
            .as_ref()
            .is_some_and(|value| trusted_environments.contains(value));
    let rule_capabilities = match decode_string_list(&rule.get::<String, _>("capabilities")) {
        Ok(values) => values,
        Err(error) => return internal_error(error),
    };
    let capabilities = oidc_capabilities(&rule_capabilities, trusted);
    let run_scope = format!(
        "{}:{}:{}",
        claims.repository_id, claims.run_id, claims.run_attempt
    );
    let principal_id = Uuid::new_v4();
    let principal_id_text: String = match sqlx::query_scalar(
        "INSERT INTO principals
           (id, workspace_id, kind, external_subject, display_name, last_seen_at)
         VALUES ($1, $2, 'ci', $3, $4, $5)
         ON CONFLICT (workspace_id, kind, external_subject)
         DO UPDATE SET enabled = 1, last_seen_at = excluded.last_seen_at
         RETURNING id",
    )
    .bind(uuid_text(principal_id))
    .bind(uuid_text(state.workspace_id))
    .bind(&claims.sub)
    .bind(&claims.repository)
    .bind(unix_now())
    .fetch_one(&state.pool)
    .await
    {
        Ok(id) => id,
        Err(error) => return internal_error(error.into()),
    };
    let principal_id = match parse_uuid(&principal_id_text) {
        Ok(id) => id,
        Err(error) => return internal_error(error),
    };
    let raw_token = format!(
        "gomo_oidc_{}{}",
        Uuid::new_v4().simple(),
        Uuid::new_v4().simple()
    );
    let token_hash = blake3::hash(raw_token.as_bytes());
    let public_prefix = raw_token.chars().take(12).collect::<String>();
    let ttl = state.config.oidc_token_ttl_seconds.clamp(60, 3600);
    let capabilities_json = match encode_string_list(&capabilities) {
        Ok(value) => value,
        Err(error) => return internal_error(error),
    };
    let expires_at = unix_now() + ttl;
    if let Err(error) = sqlx::query(
        "INSERT INTO access_tokens
           (id, public_prefix, principal_id, token_hash, capabilities, expires_at,
            allowed_run_scope, source_repository, source_ref, source_commit,
            source_run)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
    )
    .bind(uuid_text(Uuid::new_v4()))
    .bind(public_prefix)
    .bind(uuid_text(principal_id))
    .bind(token_hash.as_bytes().as_slice())
    .bind(capabilities_json)
    .bind(expires_at)
    .bind(&run_scope)
    .bind(&claims.repository)
    .bind(claims.r#ref.as_deref())
    .bind(&claims.sha)
    .bind(format!("{}:{}", claims.run_id, claims.run_attempt))
    .execute(&state.pool)
    .await
    {
        return internal_error(error.into());
    }
    Json(GithubOidcExchangeResponse {
        token: raw_token,
        expires_at_unix_seconds: expires_at,
        capabilities,
        run_scope,
    })
    .into_response()
}

fn oidc_capabilities(rule_capabilities: &[String], trusted: bool) -> Vec<String> {
    rule_capabilities
        .iter()
        .filter(|capability| trusted || capability.as_str() != "cache:shared:write")
        .cloned()
        .collect()
}

async fn validate_github_jwt(state: &AppState, jwt: &str) -> Result<GithubClaims> {
    let header = decode_header(jwt).context("invalid JWT header")?;
    if header.alg != Algorithm::RS256 {
        bail!("GitHub OIDC JWT must use RS256");
    }
    let key_id = header.kid.context("GitHub OIDC JWT is missing kid")?;
    let jwks = github_jwks(state).await?;
    let jwk = jwks
        .keys
        .iter()
        .find(|jwk| jwk.common.key_id.as_deref() == Some(&key_id))
        .context("GitHub OIDC signing key was not found")?;
    let key = DecodingKey::from_jwk(jwk)?;
    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_issuer(&[state.config.oidc_issuer.as_str()]);
    validation.validate_aud = false;
    validation.required_spec_claims = ["exp", "iss", "sub"]
        .into_iter()
        .map(str::to_string)
        .collect();
    let claims = decode::<GithubClaims>(jwt, &key, &validation)?.claims;
    if claims.exp <= 0 || claims.iss != state.config.oidc_issuer {
        bail!("GitHub OIDC claims are invalid");
    }
    Ok(claims)
}

async fn github_jwks(state: &AppState) -> Result<JwkSet> {
    {
        let cached = state.oidc.jwks.read().await;
        if let Some((fetched_at, keys)) = &*cached
            && fetched_at.elapsed() < Duration::from_secs(3600)
        {
            return Ok(keys.clone());
        }
    }
    let keys = state
        .oidc
        .client
        .get(&state.config.oidc_jwks_url)
        .send()
        .await?
        .error_for_status()?
        .json::<JwkSet>()
        .await
        .context("invalid GitHub JWK response")?;
    *state.oidc.jwks.write().await = Some((Instant::now(), keys.clone()));
    Ok(keys)
}

struct EntryRow {
    entry_id: Uuid,
    blob_id: Uuid,
    bundle_digest: String,
    result_digest: String,
    byte_len: i64,
    object_key: String,
    scope: String,
    created_at: String,
}

async fn lookup_entry(state: &AppState, task_hash: &str, scope: &str) -> Result<Option<EntryRow>> {
    let row = sqlx::query(
        "SELECT e.id AS entry_id, b.id AS blob_id, b.bundle_digest, e.result_digest,
                b.compressed_byte_length, b.object_key, s.external_scope_key,
                e.created_at
         FROM cache_entries e
         JOIN blobs b ON b.id = e.blob_id AND b.state = 'available'
         JOIN cache_scopes s ON s.id = e.scope_id
         WHERE e.workspace_id = $1 AND s.external_scope_key = $2
           AND e.protocol_version = $3 AND e.task_hash = $4",
    )
    .bind(uuid_text(state.workspace_id))
    .bind(scope_key(scope))
    .bind(PROTOCOL_VERSION)
    .bind(task_hash)
    .fetch_optional(&state.pool)
    .await?;
    row.map(|row| -> Result<EntryRow> {
        Ok(EntryRow {
            entry_id: parse_uuid(&row.get::<String, _>("entry_id"))?,
            blob_id: parse_uuid(&row.get::<String, _>("blob_id"))?,
            bundle_digest: row.get("bundle_digest"),
            result_digest: row.get("result_digest"),
            byte_len: row.get("compressed_byte_length"),
            object_key: row.get("object_key"),
            scope: row.get("external_scope_key"),
            created_at: format_unix_timestamp(row.get("created_at")),
        })
    })
    .transpose()
}

async fn head_cache(
    State(state): State<Arc<AppState>>,
    Path(task_hash): Path<String>,
    headers: HeaderMap,
) -> Response {
    let (scope, _) = match authorize(&state, &headers, Some(false), false).await {
        Ok(auth) => auth,
        Err(response) => return response,
    };
    match lookup_entry(&state, &task_hash, &scope).await {
        Ok(Some(entry)) => {
            state.metrics.hits.fetch_add(1, Ordering::Relaxed);
            entry_headers(StatusCode::OK, &entry, Body::empty())
        }
        Ok(None) => {
            state.metrics.misses.fetch_add(1, Ordering::Relaxed);
            StatusCode::NOT_FOUND.into_response()
        }
        Err(error) => internal_error(error),
    }
}

async fn get_cache(
    State(state): State<Arc<AppState>>,
    Path(task_hash): Path<String>,
    headers: HeaderMap,
) -> Response {
    let (scope, _) = match authorize(&state, &headers, Some(false), false).await {
        Ok(auth) => auth,
        Err(response) => return response,
    };
    let entry = match lookup_entry(&state, &task_hash, &scope).await {
        Ok(Some(entry)) => entry,
        Ok(None) => {
            state.metrics.misses.fetch_add(1, Ordering::Relaxed);
            return StatusCode::NOT_FOUND.into_response();
        }
        Err(error) => return internal_error(error),
    };
    state.metrics.hits.fetch_add(1, Ordering::Relaxed);
    state
        .metrics
        .download_bytes
        .fetch_add(entry.byte_len as u64, Ordering::Relaxed);
    let object = state
        .s3
        .get_object()
        .bucket(&state.config.bucket)
        .key(&entry.object_key)
        .send()
        .await;
    let body = match object {
        Ok(object) => Body::from_stream(tokio_util::io::ReaderStream::new(
            object.body.into_async_read(),
        )),
        Err(error) => {
            let _ = sqlx::query("UPDATE blobs SET state = 'failed' WHERE id = $1")
                .bind(uuid_text(entry.blob_id))
                .execute(&state.pool)
                .await;
            error!(%error, blob_id = %entry.blob_id, "published object is missing");
            return StatusCode::NOT_FOUND.into_response();
        }
    };
    let _ = sqlx::query("UPDATE cache_entries SET last_access_at = $2 WHERE id = $1")
        .bind(uuid_text(entry.entry_id))
        .bind(unix_now())
        .execute(&state.pool)
        .await;
    entry_headers(StatusCode::OK, &entry, body)
}

async fn put_cache(
    State(state): State<Arc<AppState>>,
    Path(task_hash): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let (scope, auth) = match authorize(&state, &headers, Some(true), true).await {
        Ok(auth) => auth,
        Err(response) => return response,
    };
    if !valid_task_hash(&task_hash) {
        return api_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_task_hash",
            "task hash must be 64 lowercase hexadecimal characters",
        );
    }
    let expected_length = match header_u64(&headers, HEADER_BUNDLE_LENGTH)
        .or_else(|| header_u64(&headers, "content-length"))
    {
        Some(length) if length <= state.config.max_entry_size_bytes => length,
        Some(_) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
        None => {
            return api_error(
                StatusCode::LENGTH_REQUIRED,
                "content_length_required",
                "Content-Length is required",
            );
        }
    };
    let bundle_digest = match header_string(&headers, HEADER_BUNDLE_DIGEST) {
        Some(value) => value,
        None => {
            return api_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "bundle_digest_required",
                "bundle digest is required",
            );
        }
    };
    let result_digest = match header_string(&headers, HEADER_RESULT_DIGEST) {
        Some(value) => value,
        None => {
            return api_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "result_digest_required",
                "result digest is required",
            );
        }
    };
    if let Ok(Some(existing)) = lookup_entry(&state, &task_hash, &scope).await {
        return if existing.result_digest == result_digest {
            entry_headers(StatusCode::OK, &existing, Body::empty())
        } else {
            StatusCode::CONFLICT.into_response()
        };
    }
    let spool = match tempfile::NamedTempFile::new() {
        Ok(file) => file,
        Err(error) => return internal_error(error.into()),
    };
    let spool_path = spool.path().to_path_buf();
    let (actual_length, actual_digest) = match spool_body(
        body.into_data_stream(),
        &spool_path,
        state.config.max_entry_size_bytes,
    )
    .await
    {
        Ok(result) => result,
        Err(error)
            if error
                .to_string()
                .contains("upload exceeds maximum entry size") =>
        {
            return StatusCode::PAYLOAD_TOO_LARGE.into_response();
        }
        Err(_) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "body_read_failed",
                "failed to read upload body",
            );
        }
    };
    if actual_length != expected_length {
        return api_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "length_mismatch",
            "request body length did not match Content-Length",
        );
    }
    if actual_digest != bundle_digest {
        state
            .metrics
            .digest_failures
            .fetch_add(1, Ordering::Relaxed);
        return api_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bundle_digest_mismatch",
            "bundle digest did not match request body",
        );
    }
    let validation_path = spool_path.clone();
    let validation_task_hash = task_hash.clone();
    let validation_result_digest = result_digest.clone();
    let max_expanded_bytes = max_expanded_bytes(state.config.max_entry_size_bytes);
    let validation = tokio::task::spawn_blocking(move || {
        validate_bundle(
            &validation_path,
            &validation_task_hash,
            &validation_result_digest,
            max_expanded_bytes,
        )
    })
    .await;
    if let Err(error) = validation
        .map_err(anyhow::Error::from)
        .and_then(|result| result)
    {
        state
            .metrics
            .digest_failures
            .fetch_add(1, Ordering::Relaxed);
        return api_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_bundle",
            &error.to_string(),
        );
    }
    let (scope_id, kind) = match ensure_scope(&state, &scope, auth.principal_id).await {
        Ok(value) => value,
        Err(error) => return internal_error(error),
    };
    if !auth.capabilities.contains(&format!("cache:{kind}:write")) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let upload_id = Uuid::new_v4();
    let object_key = format!(
        "{}objects/v1/{}/{}/{}.bundle",
        state.config.object_prefix,
        state.workspace_id,
        &bundle_digest[..2],
        upload_id
    );
    let upload_session = match reserve_upload(
        &state,
        auth.principal_id,
        scope_id,
        &task_hash,
        &object_key,
        expected_length,
    )
    .await
    {
        Ok(session) => session,
        Err(error) => {
            return api_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "quota_exceeded",
                &error.to_string(),
            );
        }
    };
    if let Err(error) = upload_object(&state, &object_key, &spool_path, expected_length).await {
        let _ = release_upload(&state.pool, state.workspace_id, upload_session, "failed").await;
        return internal_error(error);
    }
    match state
        .s3
        .head_object()
        .bucket(&state.config.bucket)
        .key(&object_key)
        .send()
        .await
    {
        Ok(object) if object.content_length() == Some(expected_length as i64) => {}
        Ok(_) => {
            abandon_upload(&state, upload_session, &object_key).await;
            return api_error(
                StatusCode::BAD_GATEWAY,
                "object_length_mismatch",
                "object storage did not persist the expected byte length",
            );
        }
        Err(error) => {
            abandon_upload(&state, upload_session, &object_key).await;
            return internal_error(error.into());
        }
    }
    let mut tx = match state.pool.begin().await {
        Ok(tx) => tx,
        Err(error) => {
            abandon_upload(&state, upload_session, &object_key).await;
            return internal_error(error.into());
        }
    };
    let blob_id = Uuid::new_v4();
    let now = unix_now();
    if let Err(error) = sqlx::query(
        "INSERT INTO blobs
          (id, workspace_id, bundle_digest, object_key, compressed_byte_length, state, last_verified_at)
         VALUES ($1, $2, $3, $4, $5, 'available', $6)",
    )
    .bind(uuid_text(blob_id))
    .bind(uuid_text(state.workspace_id))
    .bind(&bundle_digest)
    .bind(&object_key)
    .bind(expected_length as i64)
    .bind(now)
    .execute(&mut *tx)
    .await
    {
        drop(tx);
        abandon_upload(&state, upload_session, &object_key).await;
        return internal_error(error.into());
    }
    let entry_id = Uuid::new_v4();
    let inserted = match sqlx::query(
        "INSERT INTO cache_entries
          (id, workspace_id, scope_id, protocol_version, cache_schema_version, task_hash,
           result_digest, blob_id, producer_principal_id, source_repository,
           source_ref, source_commit, source_run)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
         ON CONFLICT (workspace_id, scope_id, protocol_version, task_hash) DO NOTHING",
    )
    .bind(uuid_text(entry_id))
    .bind(uuid_text(state.workspace_id))
    .bind(uuid_text(scope_id))
    .bind(PROTOCOL_VERSION)
    .bind(CACHE_SCHEMA_VERSION)
    .bind(&task_hash)
    .bind(&result_digest)
    .bind(uuid_text(blob_id))
    .bind(uuid_text(auth.principal_id))
    .bind(auth.source_repository.as_deref())
    .bind(auth.source_ref.as_deref())
    .bind(auth.source_commit.as_deref())
    .bind(auth.source_run.as_deref())
    .execute(&mut *tx)
    .await
    {
        Ok(result) => result.rows_affected() == 1,
        Err(error) => {
            drop(tx);
            abandon_upload(&state, upload_session, &object_key).await;
            return internal_error(error.into());
        }
    };
    if inserted {
        state
            .metrics
            .upload_bytes
            .fetch_add(expected_length, Ordering::Relaxed);
        if let Err(error) =
            finish_upload_in_transaction(&mut tx, state.workspace_id, upload_session, "complete")
                .await
        {
            drop(tx);
            abandon_upload(&state, upload_session, &object_key).await;
            return internal_error(error);
        }
        if let Err(error) = tx.commit().await {
            abandon_upload(&state, upload_session, &object_key).await;
            return internal_error(error.into());
        }
        let entry = EntryRow {
            entry_id,
            blob_id,
            bundle_digest,
            result_digest,
            byte_len: expected_length as i64,
            object_key,
            scope: scope_key(&scope).to_string(),
            created_at: format_unix_timestamp(now),
        };
        entry_headers(StatusCode::CREATED, &entry, Body::empty())
    } else {
        if let Err(error) = sqlx::query("UPDATE blobs SET state = 'deleting' WHERE id = $1")
            .bind(uuid_text(blob_id))
            .execute(&mut *tx)
            .await
        {
            drop(tx);
            abandon_upload(&state, upload_session, &object_key).await;
            return internal_error(error.into());
        }
        if let Err(error) =
            finish_upload_in_transaction(&mut tx, state.workspace_id, upload_session, "failed")
                .await
        {
            drop(tx);
            abandon_upload(&state, upload_session, &object_key).await;
            return internal_error(error);
        }
        if let Err(error) = tx.commit().await {
            abandon_upload(&state, upload_session, &object_key).await;
            return internal_error(error.into());
        }
        cleanup_unpublished_blob(&state, blob_id, &object_key).await;
        match lookup_entry(&state, &task_hash, &scope).await {
            Ok(Some(existing)) if existing.result_digest == result_digest => {
                entry_headers(StatusCode::OK, &existing, Body::empty())
            }
            Ok(Some(_)) => {
                state.metrics.conflicts.fetch_add(1, Ordering::Relaxed);
                StatusCode::CONFLICT.into_response()
            }
            Ok(None) => internal_error(anyhow::anyhow!("publication race produced no winner")),
            Err(error) => internal_error(error),
        }
    }
}

async fn abandon_upload(state: &AppState, upload_session: Uuid, object_key: &str) {
    if let Err(error) =
        release_upload(&state.pool, state.workspace_id, upload_session, "failed").await
    {
        error!(%error, %upload_session, "failed to release upload quota");
    }
    if let Err(error) = state
        .s3
        .delete_object()
        .bucket(&state.config.bucket)
        .key(object_key)
        .send()
        .await
    {
        error!(%error, %object_key, "failed to remove abandoned upload object");
    }
}

async fn cleanup_unpublished_blob(state: &AppState, blob_id: Uuid, object_key: &str) {
    if let Err(error) = state
        .s3
        .delete_object()
        .bucket(&state.config.bucket)
        .key(object_key)
        .send()
        .await
    {
        error!(%error, %blob_id, %object_key, "failed to remove losing upload object");
        return;
    }
    if let Err(error) = sqlx::query(
        "DELETE FROM blobs
         WHERE id = $1
           AND NOT EXISTS (SELECT 1 FROM cache_entries e WHERE e.blob_id = blobs.id)",
    )
    .bind(uuid_text(blob_id))
    .execute(&state.pool)
    .await
    {
        error!(%error, %blob_id, "failed to remove losing upload metadata");
    }
}

async fn reserve_upload(
    state: &AppState,
    principal_id: Uuid,
    scope_id: Uuid,
    task_hash: &str,
    object_key: &str,
    byte_length: u64,
) -> Result<Uuid> {
    let requested = i64::try_from(byte_length).context("entry is too large")?;
    let mut tx = state.pool.begin().await?;
    let reserved = sqlx::query(
        "UPDATE workspaces
         SET reserved_bytes = reserved_bytes + $2
         WHERE id = $1
           AND reserved_bytes + $2 + COALESCE((
             SELECT SUM(compressed_byte_length)
             FROM blobs WHERE workspace_id = $1 AND state = 'available'
           ), 0) <= total_storage_quota",
    )
    .bind(uuid_text(state.workspace_id))
    .bind(requested)
    .execute(&mut *tx)
    .await?;
    if reserved.rows_affected() != 1 {
        bail!("workspace storage quota would be exceeded");
    }
    let session = Uuid::new_v4();
    let expires_at = unix_now() + 3600;
    sqlx::query(
        "INSERT INTO upload_sessions
          (id, workspace_id, scope_id, principal_id, task_hash, object_key,
           expected_byte_length, reserved_quota_bytes, state, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $7, 'uploading', $8)",
    )
    .bind(uuid_text(session))
    .bind(uuid_text(state.workspace_id))
    .bind(uuid_text(scope_id))
    .bind(uuid_text(principal_id))
    .bind(task_hash)
    .bind(object_key)
    .bind(requested)
    .bind(expires_at)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(session)
}

async fn finish_upload_in_transaction(
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    workspace_id: Uuid,
    session: Uuid,
    upload_state: &str,
) -> Result<()> {
    let reserved: Option<i64> = sqlx::query_scalar(
        "SELECT reserved_quota_bytes
         FROM upload_sessions WHERE id = $1 AND state = 'uploading'",
    )
    .bind(uuid_text(session))
    .fetch_optional(&mut **tx)
    .await?;
    let reserved = reserved.context("upload session is no longer active")?;
    let updated = sqlx::query(
        "UPDATE upload_sessions
         SET state = $2, completed_at = $3, reserved_quota_bytes = 0
         WHERE id = $1 AND state = 'uploading'",
    )
    .bind(uuid_text(session))
    .bind(upload_state)
    .bind(unix_now())
    .execute(&mut **tx)
    .await?;
    if updated.rows_affected() != 1 {
        bail!("upload session is no longer active");
    }
    sqlx::query(
        "UPDATE workspaces
         SET reserved_bytes = max(0, reserved_bytes - $2)
         WHERE id = $1",
    )
    .bind(uuid_text(workspace_id))
    .bind(reserved)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn release_upload(
    pool: &SqlitePool,
    workspace_id: Uuid,
    session: Uuid,
    upload_state: &str,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    finish_upload_in_transaction(&mut tx, workspace_id, session, upload_state).await?;
    tx.commit().await?;
    Ok(())
}

async fn ensure_scope(
    state: &AppState,
    requested: &str,
    principal_id: Uuid,
) -> Result<(Uuid, &'static str)> {
    let (kind, key) = parse_scope(requested)?;
    let expires_at = if kind == "run" {
        Some(unix_now() + 7 * 24 * 3600)
    } else {
        None
    };
    let owner = if kind == "private" {
        Some(uuid_text(principal_id))
    } else {
        None
    };
    let scope_id = Uuid::new_v4();
    let id_text: String = sqlx::query_scalar(
        "INSERT INTO cache_scopes
           (id, workspace_id, kind, external_scope_key, owner_principal_id, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT (workspace_id, kind, external_scope_key)
         DO UPDATE SET external_scope_key = excluded.external_scope_key,
                       expires_at = excluded.expires_at
         RETURNING id",
    )
    .bind(uuid_text(scope_id))
    .bind(uuid_text(state.workspace_id))
    .bind(kind)
    .bind(key)
    .bind(owner)
    .bind(expires_at)
    .fetch_one(&state.pool)
    .await?;
    Ok((parse_uuid(&id_text)?, kind))
}

async fn authorize(
    state: &AppState,
    headers: &HeaderMap,
    write: Option<bool>,
    require_scope: bool,
) -> std::result::Result<(String, AuthContext), Response> {
    if header_string(headers, HEADER_WORKSPACE).as_deref() != Some(&state.config.workspace) {
        return Err(StatusCode::FORBIDDEN.into_response());
    }
    if header_string(headers, HEADER_PROTOCOL_VERSION).as_deref() != Some(PROTOCOL_VERSION) {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "protocol_mismatch",
            "unsupported Gomo cache protocol",
        ));
    }
    let token = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let Some(token) = token else {
        return Err(StatusCode::UNAUTHORIZED.into_response());
    };
    let presented = blake3::hash(token.as_bytes());
    let prefix8 = token.chars().take(8).collect::<String>();
    let prefix12 = token.chars().take(12).collect::<String>();
    let now = unix_now();
    let candidates = match sqlx::query(
        "SELECT t.id, t.token_hash, t.capabilities, t.allowed_run_scope,
                t.source_repository, t.source_ref, t.source_commit, t.source_run,
                p.id AS principal_id
         FROM access_tokens t
         JOIN principals p ON p.id = t.principal_id
         JOIN workspaces w ON w.id = p.workspace_id
         WHERE t.public_prefix IN ($1, $2)
           AND p.workspace_id = $3
           AND p.enabled = 1 AND w.enabled = 1
           AND t.revoked_at IS NULL
           AND (t.expires_at IS NULL OR t.expires_at > $4)",
    )
    .bind(prefix8)
    .bind(prefix12)
    .bind(uuid_text(state.workspace_id))
    .bind(now)
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows,
        Err(error) => return Err(internal_error(error.into())),
    };
    let mut authenticated = None;
    for candidate in candidates {
        let stored: Vec<u8> = candidate.get("token_hash");
        if stored.len() == 32 && bool::from(presented.as_bytes().ct_eq(stored.as_slice())) {
            let capabilities = match decode_string_list(&candidate.get::<String, _>("capabilities"))
            {
                Ok(values) => values.into_iter().collect(),
                Err(error) => return Err(internal_error(error)),
            };
            let principal_id = match parse_uuid(&candidate.get::<String, _>("principal_id")) {
                Ok(id) => id,
                Err(error) => return Err(internal_error(error)),
            };
            let token_id = match parse_uuid(&candidate.get::<String, _>("id")) {
                Ok(id) => id,
                Err(error) => return Err(internal_error(error)),
            };
            authenticated = Some((
                token_id,
                AuthContext {
                    principal_id,
                    capabilities,
                    allowed_run_scope: candidate.get("allowed_run_scope"),
                    source_repository: candidate.get("source_repository"),
                    source_ref: candidate.get("source_ref"),
                    source_commit: candidate.get("source_commit"),
                    source_run: candidate.get("source_run"),
                },
            ));
            break;
        }
    }
    let Some((token_id, auth)) = authenticated else {
        return Err(StatusCode::UNAUTHORIZED.into_response());
    };
    let _ = sqlx::query("UPDATE access_tokens SET last_used_at = $2 WHERE id = $1")
        .bind(uuid_text(token_id))
        .bind(now)
        .execute(&state.pool)
        .await;
    let scope = header_string(headers, HEADER_SCOPE).unwrap_or_else(|| "shared".to_string());
    let (kind, scope_key) = match parse_scope(&scope) {
        Ok(value) => value,
        Err(_) => return Err(StatusCode::FORBIDDEN.into_response()),
    };
    if kind == "private" && scope_key != auth.principal_id.to_string() {
        return Err(StatusCode::FORBIDDEN.into_response());
    }
    if kind == "run"
        && auth
            .allowed_run_scope
            .as_deref()
            .is_some_and(|allowed| allowed != scope_key)
    {
        return Err(StatusCode::FORBIDDEN.into_response());
    }
    if let Some(write) = write {
        let capability = format!("cache:{kind}:{}", if write { "write" } else { "read" });
        if !auth.capabilities.contains(&capability) {
            return Err(StatusCode::FORBIDDEN.into_response());
        }
    } else if require_scope {
        return Err(StatusCode::BAD_REQUEST.into_response());
    }
    Ok((scope, auth))
}

fn parse_scope(scope: &str) -> Result<(&'static str, &str)> {
    if scope == "shared" {
        Ok(("shared", "shared"))
    } else if let Some(key) = scope.strip_prefix("run:").filter(|key| !key.is_empty()) {
        Ok(("run", key))
    } else if let Some(key) = scope.strip_prefix("private:").filter(|key| !key.is_empty()) {
        Ok(("private", key))
    } else {
        bail!("invalid cache scope")
    }
}

fn scope_key(scope: &str) -> &str {
    parse_scope(scope).map(|(_, key)| key).unwrap_or(scope)
}

#[derive(Deserialize)]
struct BundleManifest {
    protocol_version: String,
    cache_schema_version: String,
    task_hash: String,
    result_digest: String,
    files: BTreeMap<String, Artifact>,
    total_expanded_bytes: u64,
}

#[derive(Deserialize)]
struct Artifact {
    blake3: String,
    byte_len: u64,
}

#[derive(Deserialize)]
struct EntryMetadata {
    schema_version: String,
    hash: String,
    result_digest: String,
}

struct ActualArtifact {
    blake3: String,
    byte_len: u64,
}

async fn spool_body(
    mut body: BodyDataStream,
    path: &std::path::Path,
    max_bytes: u64,
) -> Result<(u64, String)> {
    let mut file = tokio::fs::File::create(path).await?;
    let mut hasher = blake3::Hasher::new();
    let mut byte_len = 0_u64;
    while let Some(chunk) = body.next().await {
        let chunk = chunk.context("failed to read upload body")?;
        byte_len = byte_len
            .checked_add(chunk.len() as u64)
            .context("upload length overflow")?;
        if byte_len > max_bytes {
            bail!("upload exceeds maximum entry size");
        }
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
    }
    file.sync_all().await?;
    Ok((byte_len, hasher.finalize().to_hex().to_string()))
}

fn max_expanded_bytes(max_entry_size_bytes: u64) -> u64 {
    // Absolute decompressed budget. Independent of compressed size so highly
    // compressible legitimate bundles are accepted while zip-bombs stay bounded.
    const DEFAULT_MAX_EXPANDED_BYTES: u64 = 20 * 1024 * 1024 * 1024;
    max_entry_size_bytes
        .saturating_mul(8)
        .max(DEFAULT_MAX_EXPANDED_BYTES)
}

fn validate_bundle(
    path: &std::path::Path,
    task_hash: &str,
    result_digest: &str,
    max_expanded: u64,
) -> Result<()> {
    let decoder = zstd::stream::read::Decoder::new(std::fs::File::open(path)?)?;
    let mut archive = tar::Archive::new(decoder);
    let mut manifest = None;
    let mut artifacts = BTreeMap::<String, ActualArtifact>::new();
    let mut entry_metadata = None;
    let mut expanded = 0_u64;
    let mut seen_paths = BTreeSet::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() {
            bail!("bundle entries must be regular files");
        }
        let path = entry.path()?.into_owned();
        if path.is_absolute()
            || path
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            bail!("bundle entry path is unsafe");
        }
        if !seen_paths.insert(path.clone()) {
            bail!("bundle contains a duplicate entry path");
        }
        let path = path.to_str().context("bundle path is not UTF-8")?;
        expanded = expanded
            .checked_add(entry.size())
            .context("size overflow")?;
        if expanded > max_expanded {
            bail!("expanded bundle exceeds limit");
        }
        if path == "bundle.json" {
            let mut contents = Vec::new();
            (&mut entry)
                .take(1024 * 1024 + 1)
                .read_to_end(&mut contents)?;
            if contents.len() > 1024 * 1024 {
                bail!("bundle.json exceeds the metadata limit");
            }
            manifest = Some(serde_json::from_slice::<BundleManifest>(&contents)?);
        } else if let Some(name) = path.strip_prefix("entry/") {
            if !matches!(
                name,
                "meta.json"
                    | "hash-manifest.json"
                    | "stdout.txt"
                    | "stderr.txt"
                    | "outputs.tar.zst"
            ) {
                bail!("unexpected bundle artifact");
            }
            let mut hasher = blake3::Hasher::new();
            let mut byte_len = 0_u64;
            let mut metadata_bytes = if name == "meta.json" {
                Some(Vec::new())
            } else {
                None
            };
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                let read = entry.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                byte_len = byte_len
                    .checked_add(read as u64)
                    .context("artifact length overflow")?;
                hasher.update(&buffer[..read]);
                if let Some(bytes) = metadata_bytes.as_mut() {
                    if byte_len > 2 * 1024 * 1024 {
                        bail!("meta.json exceeds the metadata limit");
                    }
                    bytes.extend_from_slice(&buffer[..read]);
                }
            }
            if let Some(bytes) = metadata_bytes {
                entry_metadata = Some(serde_json::from_slice::<EntryMetadata>(&bytes)?);
            }
            if artifacts
                .insert(
                    name.to_string(),
                    ActualArtifact {
                        blake3: hasher.finalize().to_hex().to_string(),
                        byte_len,
                    },
                )
                .is_some()
            {
                bail!("bundle contains a duplicate artifact");
            }
        } else {
            bail!("unexpected bundle path");
        }
    }
    let manifest = manifest.context("bundle.json is missing")?;
    if manifest.protocol_version != PROTOCOL_VERSION
        || manifest.cache_schema_version != CACHE_SCHEMA_VERSION
        || manifest.task_hash != task_hash
        || manifest.result_digest != result_digest
        || manifest.total_expanded_bytes > max_expanded
    {
        bail!("bundle identity does not match request");
    }
    let entry_metadata = entry_metadata.context("entry/meta.json is missing")?;
    if entry_metadata.schema_version != CACHE_SCHEMA_VERSION
        || entry_metadata.hash != task_hash
        || entry_metadata.result_digest != result_digest
    {
        bail!("entry metadata does not match the requested task and result");
    }
    if artifacts.len() != manifest.files.len() {
        bail!("bundle artifact set does not match bundle.json");
    }
    if manifest.total_expanded_bytes
        != artifacts
            .values()
            .map(|artifact| artifact.byte_len)
            .sum::<u64>()
    {
        bail!("bundle expanded byte declaration does not match its artifacts");
    }
    for (name, expected) in manifest.files {
        let actual = artifacts.get(&name).context("bundle artifact is missing")?;
        if actual.byte_len != expected.byte_len || actual.blake3 != expected.blake3 {
            bail!("bundle artifact failed integrity validation");
        }
    }
    Ok(())
}

async fn report_corruption(
    State(state): State<Arc<AppState>>,
    Path(task_hash): Path<String>,
    headers: HeaderMap,
) -> Response {
    let (scope, auth) = match authorize(&state, &headers, Some(false), false).await {
        Ok(auth) => auth,
        Err(response) => return response,
    };
    let scope_id = ensure_scope(&state, &scope, auth.principal_id)
        .await
        .ok()
        .map(|value| value.0);
    match sqlx::query(
        "INSERT INTO cache_events
          (principal_id, workspace_id, scope_id, operation, task_hash, result, request_id)
         VALUES ($1, $2, $3, 'corruption-report', $4, 'reported', $5)",
    )
    .bind(uuid_text(auth.principal_id))
    .bind(uuid_text(state.workspace_id))
    .bind(scope_id.map(uuid_text))
    .bind(task_hash)
    .bind(header_string(&headers, "x-request-id").unwrap_or_else(|| uuid_text(Uuid::new_v4())))
    .execute(&state.pool)
    .await
    {
        Ok(_) => StatusCode::ACCEPTED.into_response(),
        Err(error) => internal_error(error.into()),
    }
}

fn entry_headers(status: StatusCode, entry: &EntryRow, body: Body) -> Response {
    let mut response = Response::builder()
        .status(status)
        .header("content-type", BUNDLE_MEDIA_TYPE)
        .header(HEADER_BUNDLE_DIGEST, &entry.bundle_digest)
        .header(HEADER_RESULT_DIGEST, &entry.result_digest)
        .header(HEADER_BUNDLE_LENGTH, entry.byte_len.to_string())
        .header(HEADER_SCOPE, &entry.scope)
        .header(HEADER_CACHE_SCHEMA, CACHE_SCHEMA_VERSION)
        .header(HEADER_CREATED_AT, &entry.created_at)
        .body(body)
        .expect("valid cache response");
    response.headers_mut().insert(
        "content-length",
        HeaderValue::from_str(&entry.byte_len.to_string()).expect("valid length"),
    );
    response
}

fn header_string(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

fn header_u64(headers: &HeaderMap, name: &str) -> Option<u64> {
    header_string(headers, name)?.parse().ok()
}

fn valid_task_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn api_error(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(ErrorResponse {
            code: code.to_string(),
            message: message.to_string(),
            request_id: None,
        }),
    )
        .into_response()
}

fn internal_error(error: anyhow::Error) -> Response {
    error!(error = ?error, "cache service request failed");
    api_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal_error",
        "the cache service could not complete the request",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_are_strictly_classified() {
        assert_eq!(parse_scope("shared").unwrap(), ("shared", "shared"));
        assert_eq!(parse_scope("run:123").unwrap(), ("run", "123"));
        assert_eq!(
            parse_scope("private:principal").unwrap(),
            ("private", "principal")
        );
        assert!(parse_scope("run:").is_err());
        assert!(parse_scope("shared:other").is_err());
        assert!(parse_scope("../shared").is_err());
    }

    #[test]
    fn untrusted_oidc_never_inherits_shared_write() {
        let configured = vec![
            "cache:shared:read".to_string(),
            "cache:shared:write".to_string(),
            "cache:run:read".to_string(),
            "cache:run:write".to_string(),
        ];
        assert!(oidc_capabilities(&configured, true).contains(&"cache:shared:write".to_string()));
        let untrusted = oidc_capabilities(&configured, false);
        assert!(!untrusted.contains(&"cache:shared:write".to_string()));
        assert!(untrusted.contains(&"cache:run:write".to_string()));
    }

    #[test]
    fn task_hashes_require_canonical_lowercase_blake3_text() {
        assert!(valid_task_hash(&"a".repeat(64)));
        assert!(!valid_task_hash(&"A".repeat(64)));
        assert!(!valid_task_hash(&"a".repeat(63)));
        assert!(!valid_task_hash(&format!("{}g", "a".repeat(63))));
    }

    #[test]
    fn bundle_validation_rejects_duplicate_archive_paths() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let encoder = zstd::stream::write::Encoder::new(file.reopen().unwrap(), 3).unwrap();
        let mut archive = tar::Builder::new(encoder);
        for _ in 0..2 {
            let bytes = b"{}";
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Regular);
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            archive
                .append_data(&mut header, "entry/stdout.txt", bytes.as_slice())
                .unwrap();
        }
        let encoder = archive.into_inner().unwrap();
        encoder.finish().unwrap();
        let error = validate_bundle(file.path(), &"a".repeat(64), &"b".repeat(64), 1024)
            .expect_err("duplicate paths must be rejected");
        assert!(error.to_string().contains("duplicate"));
    }

    #[test]
    fn string_list_json_round_trips() {
        let values = vec![
            "cache:shared:read".to_string(),
            "cache:run:write".to_string(),
            "refs/heads/main".to_string(),
        ];
        let encoded = encode_string_list(&values).unwrap();
        assert_eq!(
            encoded,
            r#"["cache:shared:read","cache:run:write","refs/heads/main"]"#
        );
        assert_eq!(decode_string_list(&encoded).unwrap(), values);
        assert_eq!(decode_string_list("[]").unwrap(), Vec::<String>::new());
        assert!(decode_string_list("not-json").is_err());
        assert!(decode_string_list(r#"[1,2]"#).is_err());
    }

    #[tokio::test]
    async fn sqlite_migration_applies_and_persists_json_lists() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("cache.db");
        let database_url = format!("sqlite:{}", db_path.display());
        let pool = connect_pool(&database_url).await.unwrap();
        run_migrations(&pool).await.unwrap();

        let workspace_id = Uuid::new_v4();
        let rule_id = Uuid::new_v4();
        let capabilities = vec![
            "cache:shared:read".to_string(),
            "cache:shared:write".to_string(),
            "cache:run:read".to_string(),
            "cache:run:write".to_string(),
        ];
        let trusted_refs = vec!["refs/heads/main".to_string()];
        let trusted_environments: Vec<String> = Vec::new();

        sqlx::query(
            "INSERT INTO workspaces (id, slug, maximum_entry_size)
             VALUES ($1, $2, $3)",
        )
        .bind(uuid_text(workspace_id))
        .bind("wooli")
        .bind(1024_i64)
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO oidc_trust_rules
               (id, workspace_id, issuer, audience, repository_owner_id, repository_id,
                reusable_workflow_ref, trusted_refs, trusted_environments, capabilities)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(uuid_text(rule_id))
        .bind(uuid_text(workspace_id))
        .bind("https://token.actions.githubusercontent.com")
        .bind("https://cache.example.internal")
        .bind("1234567")
        .bind("7654321")
        .bind(".github/workflows/ci.yml@refs/heads/main")
        .bind(encode_string_list(&trusted_refs).unwrap())
        .bind(encode_string_list(&trusted_environments).unwrap())
        .bind(encode_string_list(&capabilities).unwrap())
        .execute(&pool)
        .await
        .unwrap();

        let row = sqlx::query(
            "SELECT trusted_refs, trusted_environments, capabilities
             FROM oidc_trust_rules WHERE id = $1",
        )
        .bind(uuid_text(rule_id))
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(
            decode_string_list(&row.get::<String, _>("trusted_refs")).unwrap(),
            trusted_refs
        );
        assert_eq!(
            decode_string_list(&row.get::<String, _>("trusted_environments")).unwrap(),
            trusted_environments
        );
        assert_eq!(
            decode_string_list(&row.get::<String, _>("capabilities")).unwrap(),
            capabilities
        );

        // Foreign keys must be enforced with the configured pool options.
        let orphan = sqlx::query(
            "INSERT INTO principals
               (id, workspace_id, kind, external_subject, display_name)
             VALUES ($1, $2, 'service', 'missing-workspace', 'bad')",
        )
        .bind(uuid_text(Uuid::new_v4()))
        .bind(uuid_text(Uuid::new_v4()))
        .execute(&pool)
        .await;
        assert!(orphan.is_err());
    }

    #[tokio::test]
    async fn first_writer_wins_and_quota_release_are_transactional() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("cache.db");
        let database_url = format!("sqlite:{}", db_path.display());
        let pool = connect_pool(&database_url).await.unwrap();
        run_migrations(&pool).await.unwrap();

        let workspace_id = Uuid::new_v4();
        let principal_id = Uuid::new_v4();
        let scope_id = Uuid::new_v4();
        let now = unix_now();

        sqlx::query(
            "INSERT INTO workspaces
               (id, slug, maximum_entry_size, total_storage_quota, reserved_bytes)
             VALUES ($1, 'wooli', 1024, 10000, 0)",
        )
        .bind(uuid_text(workspace_id))
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO principals
               (id, workspace_id, kind, external_subject, display_name)
             VALUES ($1, $2, 'service', 'test', 'test')",
        )
        .bind(uuid_text(principal_id))
        .bind(uuid_text(workspace_id))
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO cache_scopes
               (id, workspace_id, kind, external_scope_key)
             VALUES ($1, $2, 'shared', 'shared')",
        )
        .bind(uuid_text(scope_id))
        .bind(uuid_text(workspace_id))
        .execute(&pool)
        .await
        .unwrap();

        let session = Uuid::new_v4();
        sqlx::query("UPDATE workspaces SET reserved_bytes = 500 WHERE id = $1")
            .bind(uuid_text(workspace_id))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO upload_sessions
               (id, workspace_id, scope_id, principal_id, task_hash, object_key,
                expected_byte_length, reserved_quota_bytes, state, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6, 500, 500, 'uploading', $7)",
        )
        .bind(uuid_text(session))
        .bind(uuid_text(workspace_id))
        .bind(uuid_text(scope_id))
        .bind(uuid_text(principal_id))
        .bind("a".repeat(64))
        .bind("objects/test.bundle")
        .bind(now + 3600)
        .execute(&pool)
        .await
        .unwrap();

        release_upload(&pool, workspace_id, session, "complete")
            .await
            .unwrap();
        let reserved: i64 =
            sqlx::query_scalar("SELECT reserved_bytes FROM workspaces WHERE id = $1")
                .bind(uuid_text(workspace_id))
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(reserved, 0);
        let state: String = sqlx::query_scalar("SELECT state FROM upload_sessions WHERE id = $1")
            .bind(uuid_text(session))
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(state, "complete");

        let blob_a = Uuid::new_v4();
        let blob_b = Uuid::new_v4();
        let task_hash = "b".repeat(64);
        for (blob_id, key) in [(blob_a, "objects/a.bundle"), (blob_b, "objects/b.bundle")] {
            sqlx::query(
                "INSERT INTO blobs
                   (id, workspace_id, bundle_digest, object_key, compressed_byte_length, state)
                 VALUES ($1, $2, $3, $4, 10, 'available')",
            )
            .bind(uuid_text(blob_id))
            .bind(uuid_text(workspace_id))
            .bind(format!("digest-{key}"))
            .bind(key)
            .execute(&pool)
            .await
            .unwrap();
        }

        let first = sqlx::query(
            "INSERT INTO cache_entries
               (id, workspace_id, scope_id, protocol_version, cache_schema_version, task_hash,
                result_digest, blob_id, producer_principal_id)
             VALUES ($1, $2, $3, $4, $5, $6, 'result-a', $7, $8)
             ON CONFLICT (workspace_id, scope_id, protocol_version, task_hash) DO NOTHING",
        )
        .bind(uuid_text(Uuid::new_v4()))
        .bind(uuid_text(workspace_id))
        .bind(uuid_text(scope_id))
        .bind(PROTOCOL_VERSION)
        .bind(CACHE_SCHEMA_VERSION)
        .bind(&task_hash)
        .bind(uuid_text(blob_a))
        .bind(uuid_text(principal_id))
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(first.rows_affected(), 1);

        let second = sqlx::query(
            "INSERT INTO cache_entries
               (id, workspace_id, scope_id, protocol_version, cache_schema_version, task_hash,
                result_digest, blob_id, producer_principal_id)
             VALUES ($1, $2, $3, $4, $5, $6, 'result-b', $7, $8)
             ON CONFLICT (workspace_id, scope_id, protocol_version, task_hash) DO NOTHING",
        )
        .bind(uuid_text(Uuid::new_v4()))
        .bind(uuid_text(workspace_id))
        .bind(uuid_text(scope_id))
        .bind(PROTOCOL_VERSION)
        .bind(CACHE_SCHEMA_VERSION)
        .bind(&task_hash)
        .bind(uuid_text(blob_b))
        .bind(uuid_text(principal_id))
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(second.rows_affected(), 0);

        let winner: String = sqlx::query_scalar(
            "SELECT result_digest FROM cache_entries
             WHERE workspace_id = $1 AND task_hash = $2",
        )
        .bind(uuid_text(workspace_id))
        .bind(&task_hash)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(winner, "result-a");
    }
}
