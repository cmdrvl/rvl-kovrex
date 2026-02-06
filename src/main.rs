//! rvl-kovrex: Kovrex agent wrapper for rvl
//!
//! This is CMD+RVL's reference implementation showing how to wrap a CLI tool
//! as a Kovrex agent with a REST API.
//!
//! ## Environment Variables
//! - `RVL_PORT` - Port to listen on (default: 8080)
//! - `RVL_HOST` - Host to bind to (default: 0.0.0.0)
//! - `RVL_API_TOKEN` - Bearer token for authentication (required in production)
//!
//! ## Endpoints
//! - `GET /health` - Health check (unauthenticated)
//! - `POST /compare` - Compare two CSVs via JSON (requires bearer token if configured)

use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use rvl::cli::args::Args;
use rvl::cli::exit::Outcome;
use rvl::orchestrator;

/// Server configuration from environment.
#[derive(Clone)]
struct Config {
    port: u16,
    host: String,
    api_token: Option<String>,
}

impl Config {
    fn from_env() -> Self {
        Self {
            port: std::env::var("RVL_PORT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(8080),
            host: std::env::var("RVL_HOST").unwrap_or_else(|_| "0.0.0.0".to_string()),
            api_token: std::env::var("RVL_API_TOKEN").ok().filter(|s| !s.is_empty()),
        }
    }
}

#[tokio::main]
async fn main() {
    // Initialize tracing
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rvl_kovrex=info,tower_http=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let config = Config::from_env();
    let addr: SocketAddr = format!("{}:{}", config.host, config.port)
        .parse()
        .expect("Invalid address");

    if config.api_token.is_some() {
        tracing::info!("API token authentication enabled");
    } else {
        tracing::warn!("No RVL_API_TOKEN set - API is unauthenticated");
    }

    let shared_config = Arc::new(config);

    let app = Router::new()
        .route("/health", get(health))
        .route("/compare", post(compare))
        .with_state(shared_config)
        .layer(DefaultBodyLimit::max(50 * 1024 * 1024)) // 50MB max
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http());

    tracing::info!("rvl-kovrex listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

/// Health check endpoint.
async fn health() -> impl IntoResponse {
    Json(HealthResponse {
        status: "ok",
        agent: "rvl",
        operator: "cmd-rvl",
        version: env!("CARGO_PKG_VERSION"),
    })
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    agent: &'static str,
    operator: &'static str,
    version: &'static str,
}

/// Kovrex file API base URL
const KOVREX_FILE_API: &str = "https://gateway.kovrex.ai/v1/files";

/// JSON request body for comparison.
#[derive(Deserialize)]
struct CompareRequest {
    /// Base64-encoded old CSV content
    #[serde(default)]
    old: Option<String>,
    /// Base64-encoded new CSV content
    #[serde(default)]
    new: Option<String>,
    /// File ID from upload_file for old CSV (e.g., kvx_file_xxx)
    #[serde(default)]
    old_file_id: Option<String>,
    /// File ID from upload_file for new CSV (e.g., kvx_file_xxx)
    #[serde(default)]
    new_file_id: Option<String>,
    /// Optional: Column name for row alignment
    #[serde(default)]
    key: Option<String>,
    /// Optional: Coverage threshold (0-1, default 0.95)
    #[serde(default = "default_threshold")]
    threshold: f64,
    /// Optional: Numeric tolerance (default 1e-9)
    #[serde(default = "default_tolerance")]
    tolerance: f64,
    /// Optional: Force delimiter
    #[serde(default)]
    delimiter: Option<String>,
}

fn default_threshold() -> f64 { 0.95 }
fn default_tolerance() -> f64 { 1e-9 }

/// Fetch file content from Kovrex file API by file_id.
async fn fetch_file_by_id(
    file_id: &str,
    auth_header: Option<&str>,
) -> Result<Vec<u8>, (StatusCode, String)> {
    tracing::info!("Fetching file: {}", file_id);

    let url = format!("{}/{}", KOVREX_FILE_API, file_id);
    let client = reqwest::Client::new();
    let mut request = client.get(&url);

    if let Some(auth) = auth_header {
        request = request.header("Authorization", auth);
    }

    let response = request.send().await.map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            format!("Failed to fetch file '{}': {}", file_id, e),
        )
    })?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err((
            StatusCode::BAD_GATEWAY,
            format!("Kovrex file API returned {} for '{}': {}", status, file_id, body),
        ));
    }

    let file_bytes = response.bytes().await.map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            format!("Failed to read file '{}': {}", file_id, e),
        )
    })?;

    tracing::info!("Fetched {} bytes for {}", file_bytes.len(), file_id);
    Ok(file_bytes.to_vec())
}

/// Resolve CSV input from either file_id or base64 content.
/// 
/// Priority: file_id > content (base64)
/// If content is base64-encoded kvx_file_xxx, also fetches from API.
async fn resolve_csv_input(
    file_id: Option<&str>,
    content: Option<&str>,
    field_name: &str,
    auth_header: Option<&str>,
) -> Result<Vec<u8>, (StatusCode, String)> {
    // If file_id is provided, use it directly
    if let Some(fid) = file_id {
        if !fid.starts_with("kvx_file_") {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("Invalid file_id format for '{}': must start with kvx_file_", field_name),
            ));
        }
        return fetch_file_by_id(fid, auth_header).await;
    }

    // Otherwise decode base64 content
    let encoded = content.ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            format!("Either {}_file_id or {} must be provided", field_name, field_name),
        )
    })?;

    let bytes = BASE64.decode(encoded).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("Invalid base64 for '{}': {}", field_name, e),
        )
    })?;

    // Check if decoded content is a file reference (backwards compat)
    let content_str = String::from_utf8_lossy(&bytes);
    if content_str.starts_with("kvx_file_") {
        return fetch_file_by_id(content_str.trim(), auth_header).await;
    }

    Ok(bytes)
}

/// Compare two CSV files.
///
/// Accepts JSON with base64-encoded CSV content:
/// ```json
/// {
///   "old": "base64-encoded-csv",
///   "new": "base64-encoded-csv",
///   "key": "id",
///   "threshold": 0.95,
///   "tolerance": 1e-9
/// }
/// ```
///
/// Requires `Authorization: Bearer <token>` header if `RVL_API_TOKEN` is set.
async fn compare(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    Json(payload): Json<CompareRequest>,
) -> impl IntoResponse {
    // Get auth header for potential file API calls
    let auth_header = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok());

    // Check bearer token if configured
    if let Some(expected_token) = &config.api_token {
        let provided_token = auth_header
            .unwrap_or("")
            .strip_prefix("Bearer ")
            .or_else(|| auth_header.unwrap_or("").strip_prefix("bearer "))
            .unwrap_or("");
        
        if provided_token != expected_token {
            return (
                StatusCode::UNAUTHORIZED,
                Json(ErrorResponse {
                    error: "Invalid or missing bearer token".to_string(),
                }),
            )
                .into_response();
        }
    }

    // Resolve old CSV (file_id takes priority over base64 content)
    let old_bytes = match resolve_csv_input(
        payload.old_file_id.as_deref(),
        payload.old.as_deref(),
        "old",
        auth_header,
    ).await {
        Ok(bytes) => bytes,
        Err((status, error)) => {
            return (status, Json(ErrorResponse { error })).into_response();
        }
    };

    // Resolve new CSV (file_id takes priority over base64 content)
    let new_bytes = match resolve_csv_input(
        payload.new_file_id.as_deref(),
        payload.new.as_deref(),
        "new",
        auth_header,
    ).await {
        Ok(bytes) => bytes,
        Err((status, error)) => {
            return (status, Json(ErrorResponse { error })).into_response();
        }
    };

    // Write old CSV to temp file
    let mut old_temp = match NamedTempFile::new() {
        Ok(t) => t,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: format!("Failed to create temp file: {}", e),
                }),
            )
                .into_response();
        }
    };
    if let Err(e) = old_temp.write_all(&old_bytes) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Failed to write temp file: {}", e),
            }),
        )
            .into_response();
    }

    // Write new CSV to temp file
    let mut new_temp = match NamedTempFile::new() {
        Ok(t) => t,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: format!("Failed to create temp file: {}", e),
                }),
            )
                .into_response();
        }
    };
    if let Err(e) = new_temp.write_all(&new_bytes) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Failed to write temp file: {}", e),
            }),
        )
            .into_response();
    }

    // Parse delimiter if provided
    let delimiter = payload.delimiter.as_ref().and_then(|s| parse_delimiter(s));

    // Build args for orchestrator
    let args = Args::new(
        PathBuf::from(old_temp.path()),
        PathBuf::from(new_temp.path()),
        payload.key,
        payload.threshold,
        payload.tolerance,
        delimiter,
        true, // Always return JSON from API
    );

    // Run comparison
    match orchestrator::run(&args) {
        Ok(result) => {
            let status = match result.outcome {
                Outcome::NoRealChange => StatusCode::OK,
                Outcome::RealChange => StatusCode::OK,
                Outcome::Refusal => StatusCode::UNPROCESSABLE_ENTITY,
            };

            // Parse the JSON output and return it
            match serde_json::from_str::<serde_json::Value>(&result.output) {
                Ok(json) => (status, Json(json)).into_response(),
                Err(_) => {
                    // Fallback: return raw output wrapped in JSON
                    (
                        status,
                        Json(serde_json::json!({
                            "raw_output": result.output
                        })),
                    )
                        .into_response()
                }
            }
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Comparison failed: {}", e),
            }),
        )
            .into_response(),
    }
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

/// Parse delimiter string to byte.
fn parse_delimiter(s: &str) -> Option<u8> {
    match s.to_lowercase().as_str() {
        "comma" | "," => Some(b','),
        "tab" | "\t" => Some(b'\t'),
        "semicolon" | ";" => Some(b';'),
        "pipe" | "|" => Some(b'|'),
        "caret" | "^" => Some(b'^'),
        _ if s.starts_with("0x") || s.starts_with("0X") => {
            u8::from_str_radix(&s[2..], 16).ok()
        }
        _ if s.len() == 1 => s.bytes().next(),
        _ => None,
    }
}
