use std::env;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use gomo_cache_protocol::{
    BUNDLE_MEDIA_TYPE, GithubOidcExchangeRequest, GithubOidcExchangeResponse, HEADER_BUNDLE_DIGEST,
    HEADER_BUNDLE_LENGTH, HEADER_CACHE_SCHEMA, HEADER_PROTOCOL_VERSION, HEADER_RESULT_DIGEST,
    HEADER_SCOPE, HEADER_WORKSPACE, PROTOCOL_VERSION, PutOutcome,
};
use reqwest::StatusCode;
use reqwest::blocking::{Client, Response};
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE};
use serde::Deserialize;

use crate::cache::{self, NamedTaskCacheDescriptor, TaskHash};
use crate::workspace::{RemoteCacheConfig, RemoteCacheMode, Workspace};

static TRANSFER_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static TRANSFER_LIMITER: OnceLock<TransferLimiter> = OnceLock::new();
static REMOTE_CLIENT_CACHE: OnceLock<Mutex<Option<CachedRemoteClient>>> = OnceLock::new();

struct CachedRemoteClient {
    config: RemoteCacheConfig,
    token_env: Option<String>,
    oidc_available: bool,
    client: Arc<RemoteCacheClient>,
}

struct TransferLimiter {
    active: Mutex<usize>,
    changed: Condvar,
}

struct TransferPermit {
    limiter: &'static TransferLimiter,
}

impl TransferPermit {
    fn acquire(maximum: usize) -> Result<Self> {
        let limiter = TRANSFER_LIMITER.get_or_init(|| TransferLimiter {
            active: Mutex::new(0),
            changed: Condvar::new(),
        });
        let mut active = limiter
            .active
            .lock()
            .map_err(|_| anyhow::anyhow!("remote transfer limiter was poisoned"))?;
        while *active >= maximum.max(1) {
            active = limiter
                .changed
                .wait(active)
                .map_err(|_| anyhow::anyhow!("remote transfer limiter was poisoned"))?;
        }
        *active += 1;
        drop(active);
        Ok(Self { limiter })
    }
}

impl Drop for TransferPermit {
    fn drop(&mut self) {
        if let Ok(mut active) = self.limiter.active.lock() {
            *active = active.saturating_sub(1);
            self.limiter.changed.notify_one();
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteHit {
    pub(crate) scope: String,
    pub(crate) bytes_downloaded: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteStore {
    pub(crate) scope: String,
    pub(crate) bytes_uploaded: u64,
    pub(crate) outcome: PutOutcome,
}

pub(crate) struct RemoteCacheClient {
    config: RemoteCacheConfig,
    client: Client,
    token: String,
    oidc_run_scope: Option<String>,
    oidc_shared_write: bool,
}

impl RemoteCacheClient {
    pub(crate) fn from_workspace(workspace: &Workspace) -> Result<Option<Arc<Self>>> {
        let Some(config) = workspace.remote_cache.clone() else {
            return Ok(None);
        };
        let token_env = match env::var("GOMO_REMOTE_CACHE_TOKEN") {
            Ok(token) if !token.trim().is_empty() => Some(token),
            Ok(_) => bail!("GOMO_REMOTE_CACHE_TOKEN must not be empty"),
            Err(_) => None,
        };
        let oidc_available = env::var_os("ACTIONS_ID_TOKEN_REQUEST_URL").is_some();
        let cache = REMOTE_CLIENT_CACHE.get_or_init(|| Mutex::new(None));
        let mut guard = cache
            .lock()
            .map_err(|_| anyhow::anyhow!("remote cache client cache was poisoned"))?;
        if let Some(cached) = guard.as_ref()
            && cached.config == config
            && cached.token_env == token_env
            && cached.oidc_available == oidc_available
        {
            return Ok(Some(cached.client.clone()));
        }
        let client = Arc::new(Self::build(
            config.clone(),
            token_env.clone(),
            oidc_available,
        )?);
        *guard = Some(CachedRemoteClient {
            config,
            token_env,
            oidc_available,
            client: client.clone(),
        });
        Ok(Some(client))
    }

    fn build(
        config: RemoteCacheConfig,
        token_env: Option<String>,
        oidc_available: bool,
    ) -> Result<Self> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(config.connect_timeout_seconds))
            .timeout(Duration::from_secs(config.request_timeout_seconds))
            .user_agent(format!("gomo/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .context("failed to create remote cache HTTP client")?;
        let (token, oidc_run_scope, oidc_shared_write) = match token_env {
            Some(token) => (token, None, false),
            None if oidc_available => {
                let exchange = github_oidc_exchange(&client, &config)?;
                let shared_write = exchange
                    .capabilities
                    .iter()
                    .any(|capability| capability == "cache:shared:write");
                (exchange.token, Some(exchange.run_scope), shared_write)
            }
            None => {
                bail!(
                    "remote cache is configured but neither GOMO_REMOTE_CACHE_TOKEN nor GitHub OIDC credentials are available"
                )
            }
        };
        Ok(Self {
            config,
            client,
            token,
            oidc_run_scope,
            oidc_shared_write,
        })
    }

    pub(crate) fn restore(
        &self,
        workspace: &Workspace,
        task_hash: &TaskHash,
    ) -> Result<Option<RemoteHit>> {
        self.restore_entry(
            workspace,
            &task_hash.hash,
            |bundle_path, bundle_digest, byte_len, max_expanded| {
                cache::import_task_bundle(
                    workspace,
                    task_hash,
                    bundle_path,
                    bundle_digest,
                    byte_len,
                    max_expanded,
                )
            },
            || cache::task_result_digest(workspace, task_hash),
        )
    }

    pub(crate) fn restore_named(
        &self,
        workspace: &Workspace,
        descriptor: &NamedTaskCacheDescriptor,
    ) -> Result<Option<RemoteHit>> {
        self.restore_entry(
            workspace,
            &descriptor.hash,
            |bundle_path, bundle_digest, byte_len, max_expanded| {
                cache::import_named_task_bundle(
                    workspace,
                    descriptor,
                    bundle_path,
                    bundle_digest,
                    byte_len,
                    max_expanded,
                )
            },
            || cache::named_task_result_digest(workspace, descriptor),
        )
    }

    fn restore_entry<Import, Digest>(
        &self,
        workspace: &Workspace,
        task_hash: &str,
        mut import: Import,
        mut local_result_digest: Digest,
    ) -> Result<Option<RemoteHit>>
    where
        Import: FnMut(&std::path::Path, &str, u64, Option<u64>) -> Result<()>,
        Digest: FnMut() -> Result<Option<String>>,
    {
        let _permit = TransferPermit::acquire(self.config.max_concurrent_transfers)?;
        for scope in self.lookup_scopes() {
            let url = self.cache_url(task_hash);
            let mut response = self
                .get_with_retry(&url, &scope)
                .with_context(|| format!("remote cache GET failed for {task_hash}"))?;
            match response.status() {
                StatusCode::NOT_FOUND => continue,
                StatusCode::OK => {}
                StatusCode::UNAUTHORIZED => bail!("remote cache authentication failed"),
                StatusCode::FORBIDDEN => {
                    bail!("remote cache denied read access to scope `{scope}`")
                }
                status => bail!("remote cache GET returned HTTP {status}"),
            }
            let bundle_digest = required_header(&response, HEADER_BUNDLE_DIGEST)?;
            let result_digest = required_header(&response, HEADER_RESULT_DIGEST)?;
            let byte_len = required_header(&response, HEADER_BUNDLE_LENGTH)?
                .parse::<u64>()
                .context("remote cache returned an invalid bundle length")?;
            if byte_len > self.config.max_entry_size_bytes {
                bail!("remote cache entry exceeds max_entry_size_bytes");
            }
            if response
                .content_length()
                .is_some_and(|length| length != byte_len)
            {
                bail!("remote cache Content-Length did not match bundle metadata");
            }
            let bundle_path = self.transfer_path(workspace, task_hash, "download")?;
            let download_result = stream_response_to_file(
                &mut response,
                &bundle_path,
                self.config.max_entry_size_bytes,
            );
            let actual_len = match download_result {
                Ok(length) => length,
                Err(error) => {
                    let _ = fs::remove_file(&bundle_path);
                    return Err(error);
                }
            };
            let import_result = import(
                &bundle_path,
                &bundle_digest,
                byte_len,
                Some(max_expanded_bytes(self.config.max_entry_size_bytes)),
            );
            let _ = fs::remove_file(&bundle_path);
            if let Err(error) = import_result {
                if is_bundle_validation_failure(&error) {
                    let _ = self.report_corruption(task_hash, &scope);
                }
                return Err(error);
            }
            let local_result_digest = local_result_digest()?
                .context("imported remote cache entry is missing result metadata")?;
            if local_result_digest != result_digest {
                bail!("remote result digest did not match the imported cache entry");
            }
            return Ok(Some(RemoteHit {
                scope,
                bytes_downloaded: actual_len,
            }));
        }
        Ok(None)
    }

    pub(crate) fn store(
        &self,
        workspace: &Workspace,
        task_hash: &TaskHash,
    ) -> Result<Option<RemoteStore>> {
        if self.config.mode == RemoteCacheMode::ReadOnly {
            return Ok(None);
        }
        let bundle = cache::export_task_bundle(workspace, task_hash)?;
        self.store_bundle(&task_hash.hash, bundle)
    }

    pub(crate) fn store_named(
        &self,
        workspace: &Workspace,
        descriptor: &NamedTaskCacheDescriptor,
    ) -> Result<Option<RemoteStore>> {
        if self.config.mode == RemoteCacheMode::ReadOnly {
            return Ok(None);
        }
        let bundle = cache::export_named_task_bundle(workspace, descriptor)?;
        self.store_bundle(&descriptor.hash, bundle)
    }

    fn store_bundle(
        &self,
        task_hash: &str,
        bundle: cache::ExportedBundle,
    ) -> Result<Option<RemoteStore>> {
        if self.config.mode == RemoteCacheMode::ReadOnly {
            return Ok(None);
        }
        let _permit = TransferPermit::acquire(self.config.max_concurrent_transfers)?;
        let scope = self.write_scope();
        let upload_result = (|| -> Result<RemoteStore> {
            let response = self
                .put_with_retry(task_hash, &scope, &bundle)
                .with_context(|| format!("remote cache PUT failed for {task_hash}"))?;
            let outcome = match response.status() {
                StatusCode::CREATED => PutOutcome::Published,
                StatusCode::OK => PutOutcome::AlreadyPublished,
                StatusCode::CONFLICT => PutOutcome::Conflict,
                StatusCode::UNAUTHORIZED => bail!("remote cache authentication failed"),
                StatusCode::FORBIDDEN => {
                    bail!("remote cache denied write access to scope `{scope}`")
                }
                StatusCode::PAYLOAD_TOO_LARGE => {
                    bail!("remote cache rejected an oversized cache entry")
                }
                StatusCode::TOO_MANY_REQUESTS => {
                    bail!("remote cache transfer limit was exceeded")
                }
                status => bail!("remote cache PUT returned HTTP {status}"),
            };
            Ok(RemoteStore {
                scope,
                bytes_uploaded: bundle.byte_len,
                outcome,
            })
        })();
        let _ = fs::remove_file(&bundle.path);
        upload_result.map(Some)
    }

    pub(crate) fn capabilities(&self) -> Result<gomo_cache_protocol::Capabilities> {
        let response = self
            .client
            .get(format!("{}/v1/capabilities", self.config.url))
            .header(AUTHORIZATION, format!("Bearer {}", self.token))
            .header(HEADER_PROTOCOL_VERSION, PROTOCOL_VERSION)
            .header(HEADER_WORKSPACE, &self.config.workspace)
            .send()
            .context("failed to query remote cache capabilities")?;
        if !response.status().is_success() {
            bail!(
                "remote cache capabilities returned HTTP {}",
                response.status()
            );
        }
        response
            .json()
            .context("remote cache returned invalid capabilities JSON")
    }

    fn request(
        &self,
        request: reqwest::blocking::RequestBuilder,
        scope: &str,
    ) -> reqwest::blocking::RequestBuilder {
        request
            .header(AUTHORIZATION, format!("Bearer {}", self.token))
            .header(HEADER_PROTOCOL_VERSION, PROTOCOL_VERSION)
            .header(HEADER_WORKSPACE, &self.config.workspace)
            .header(HEADER_SCOPE, scope)
    }

    fn get_with_retry(&self, url: &str, scope: &str) -> Result<Response> {
        self.send_with_retry(|| {
            Ok(self
                .request(self.client.get(url), scope)
                .header(ACCEPT, BUNDLE_MEDIA_TYPE)
                .send()?)
        })
    }

    fn put_with_retry(
        &self,
        task_hash: &str,
        scope: &str,
        bundle: &cache::ExportedBundle,
    ) -> Result<Response> {
        self.send_with_retry(|| {
            let file = File::open(&bundle.path)?;
            Ok(self
                .request(self.client.put(self.cache_url(task_hash)), scope)
                .header(CONTENT_TYPE, cache::remote_bundle_media_type())
                .header(CONTENT_LENGTH, bundle.byte_len)
                .header(HEADER_BUNDLE_DIGEST, &bundle.bundle_digest)
                .header(HEADER_RESULT_DIGEST, &bundle.result_digest)
                .header(HEADER_CACHE_SCHEMA, cache::CACHE_SCHEMA_VERSION)
                .body(reqwest::blocking::Body::new(file))
                .send()?)
        })
    }

    fn send_with_retry(&self, mut send: impl FnMut() -> Result<Response>) -> Result<Response> {
        const BACKOFFS: [Duration; 3] = [
            Duration::from_millis(100),
            Duration::from_millis(300),
            Duration::from_millis(900),
        ];
        let mut last_error = None;
        for attempt in 0..=BACKOFFS.len() {
            match send() {
                Ok(response) if !retryable_status(response.status()) => return Ok(response),
                Ok(response) => {
                    last_error = Some(anyhow::anyhow!(
                        "remote cache returned transient HTTP {}",
                        response.status()
                    ));
                }
                Err(error) => last_error = Some(error),
            }
            if let Some(delay) = BACKOFFS.get(attempt) {
                std::thread::sleep(*delay);
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("remote cache request failed")))
    }

    fn cache_url(&self, task_hash: &str) -> String {
        format!("{}/v1/cache/{task_hash}", self.config.url)
    }

    fn report_corruption(&self, task_hash: &str, scope: &str) -> Result<()> {
        let response = self
            .request(
                self.client.post(format!(
                    "{}/v1/cache/{task_hash}/corruption-reports",
                    self.config.url
                )),
                scope,
            )
            .send()
            .context("failed to report remote cache corruption")?;
        if !response.status().is_success() {
            bail!(
                "remote cache corruption report returned HTTP {}",
                response.status()
            );
        }
        Ok(())
    }

    fn lookup_scopes(&self) -> Vec<String> {
        let mut scopes = Vec::new();
        if let Ok(run_id) = env::var("GOMO_REMOTE_CACHE_RUN_ID")
            && !run_id.trim().is_empty()
        {
            scopes.push(format!("run:{}", run_id.trim()));
        } else if let Some(run_scope) = &self.oidc_run_scope {
            scopes.push(format!("run:{run_scope}"));
        }
        scopes.push("shared".to_string());
        scopes
    }

    fn write_scope(&self) -> String {
        if self.oidc_shared_write {
            return "shared".to_string();
        }
        env::var("GOMO_REMOTE_CACHE_RUN_ID")
            .ok()
            .filter(|run_id| !run_id.trim().is_empty())
            .map(|run_id| format!("run:{}", run_id.trim()))
            .or_else(|| {
                self.oidc_run_scope
                    .as_ref()
                    .map(|run_scope| format!("run:{run_scope}"))
            })
            .unwrap_or_else(|| "shared".to_string())
    }

    fn transfer_path(
        &self,
        workspace: &Workspace,
        task_hash: &str,
        operation: &str,
    ) -> Result<PathBuf> {
        let dir = workspace
            .cache_dir
            .join(cache::CACHE_SCHEMA_VERSION)
            .join(".transfers");
        fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
        Ok(dir.join(format!(
            "{operation}-{}-{}-{}.bundle",
            task_hash,
            std::process::id(),
            TRANSFER_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        )))
    }
}

fn retryable_status(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS
        || status == StatusCode::BAD_GATEWAY
        || status == StatusCode::SERVICE_UNAVAILABLE
        || status == StatusCode::GATEWAY_TIMEOUT
}

#[derive(Deserialize)]
struct GithubIdTokenResponse {
    value: String,
}

fn github_oidc_exchange(
    client: &Client,
    config: &RemoteCacheConfig,
) -> Result<GithubOidcExchangeResponse> {
    let request_url = env::var("ACTIONS_ID_TOKEN_REQUEST_URL")
        .context("ACTIONS_ID_TOKEN_REQUEST_URL is required for GitHub OIDC")?;
    let request_token = env::var("ACTIONS_ID_TOKEN_REQUEST_TOKEN")
        .context("ACTIONS_ID_TOKEN_REQUEST_TOKEN is required for GitHub OIDC")?;
    let audience =
        env::var("GOMO_REMOTE_CACHE_OIDC_AUDIENCE").unwrap_or_else(|_| config.url.clone());
    let mut url = reqwest::Url::parse(&request_url)
        .context("ACTIONS_ID_TOKEN_REQUEST_URL is not a valid URL")?;
    url.query_pairs_mut().append_pair("audience", &audience);
    let github_token = client
        .get(url)
        .header(AUTHORIZATION, format!("Bearer {request_token}"))
        .send()
        .context("failed to request a GitHub OIDC token")?
        .error_for_status()
        .context("GitHub rejected the OIDC token request")?
        .json::<GithubIdTokenResponse>()
        .context("GitHub returned invalid OIDC token JSON")?;
    client
        .post(format!("{}/v1/auth/github", config.url))
        .header(HEADER_PROTOCOL_VERSION, PROTOCOL_VERSION)
        .header(HEADER_WORKSPACE, &config.workspace)
        .json(&GithubOidcExchangeRequest {
            jwt: github_token.value,
        })
        .send()
        .context("failed to exchange the GitHub OIDC token")?
        .error_for_status()
        .context("remote cache rejected the GitHub OIDC token")?
        .json()
        .context("remote cache returned invalid OIDC exchange JSON")
}

fn required_header(response: &Response, name: &'static str) -> Result<String> {
    response
        .headers()
        .get(name)
        .with_context(|| format!("remote cache response is missing {name}"))?
        .to_str()
        .with_context(|| format!("remote cache response has invalid {name}"))
        .map(str::to_string)
}

fn stream_response_to_file(response: &mut Response, path: &Path, max_bytes: u64) -> Result<u64> {
    let mut file =
        File::create(path).with_context(|| format!("failed to create {}", path.display()))?;
    let mut total = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = response
            .read(&mut buffer)
            .context("failed while downloading remote cache bundle")?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .context("remote cache bundle length overflow")?;
        if total > max_bytes {
            bail!("remote cache bundle exceeds max_entry_size_bytes");
        }
        file.write_all(&buffer[..read])
            .with_context(|| format!("failed to write {}", path.display()))?;
    }
    file.sync_all()
        .with_context(|| format!("failed to sync {}", path.display()))?;
    Ok(total)
}

fn max_expanded_bytes(max_entry_size_bytes: u64) -> u64 {
    const DEFAULT_MAX_EXPANDED_BYTES: u64 = 20 * 1024 * 1024 * 1024;
    max_entry_size_bytes
        .saturating_mul(8)
        .max(DEFAULT_MAX_EXPANDED_BYTES)
}

fn is_bundle_validation_failure(error: &anyhow::Error) -> bool {
    let message = format!("{error:#}").to_lowercase();
    const MARKERS: &[&str] = &[
        "digest",
        "integrity",
        "bundle",
        "unsafe",
        "duplicate",
        "exceeds the configured expanded",
        "invalid bundle",
        "unexpected artifact",
        "missing bundle",
    ];
    MARKERS.iter().any(|marker| message.contains(marker))
        && !message.contains("failed to create")
        && !message.contains("failed to rename")
        && !message.contains("no space")
}
