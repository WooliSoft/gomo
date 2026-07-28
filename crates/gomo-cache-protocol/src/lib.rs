use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: &str = "v1";
pub const CACHE_SCHEMA_VERSION: &str = "v6";
pub const BUNDLE_MEDIA_TYPE: &str = "application/vnd.gomo.cache.v1+zstd";

pub const HEADER_PROTOCOL_VERSION: &str = "x-gomo-protocol-version";
pub const HEADER_WORKSPACE: &str = "x-gomo-workspace";
pub const HEADER_SCOPE: &str = "x-gomo-scope";
pub const HEADER_BUNDLE_DIGEST: &str = "x-gomo-bundle-digest";
pub const HEADER_RESULT_DIGEST: &str = "x-gomo-result-digest";
pub const HEADER_BUNDLE_LENGTH: &str = "x-gomo-bundle-length";
pub const HEADER_CACHE_SCHEMA: &str = "x-gomo-cache-schema";
pub const HEADER_CREATED_AT: &str = "x-gomo-created-at";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub protocol_versions: Vec<String>,
    pub cache_schema_versions: Vec<String>,
    pub max_entry_size_bytes: u64,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub code: String,
    pub message: String,
    pub request_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubOidcExchangeRequest {
    pub jwt: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubOidcExchangeResponse {
    pub token: String,
    pub expires_at_unix_seconds: i64,
    pub capabilities: Vec<String>,
    pub run_scope: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutOutcome {
    Published,
    AlreadyPublished,
    Conflict,
}
