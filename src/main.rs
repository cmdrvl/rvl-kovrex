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
//! - `POST /compare` - Compare two CSVs (requires bearer token if configured)

use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Multipart, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use serde::Serialize;
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

/// Compare two CSV files.
///
/// Accepts multipart form data with:
/// - `old`: The old CSV file
/// - `new`: The new CSV file
/// - `key`: (optional) Column name for row alignment
/// - `threshold`: (optional) Coverage threshold (0-1, default 0.95)
/// - `tolerance`: (optional) Numeric tolerance (default 1e-9)
/// - `delimiter`: (optional) Force delimiter (comma/tab/semicolon/pipe/caret)
///
/// Requires `Authorization: Bearer <token>` header if `RVL_API_TOKEN` is set.
async fn compare(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> impl IntoResponse {
    // Check bearer token if configured
    if let Some(expected_token) = &config.api_token {
        let auth_header = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        
        let provided_token = auth_header
            .strip_prefix("Bearer ")
            .or_else(|| auth_header.strip_prefix("bearer "))
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

    let mut old_file: Option<NamedTempFile> = None;
    let mut new_file: Option<NamedTempFile> = None;
    let mut key: Option<String> = None;
    let mut threshold: f64 = 0.95;
    let mut tolerance: f64 = 1e-9;
    let mut delimiter: Option<u8> = None;

    // Parse multipart form
    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        
        match name.as_str() {
            "old" => {
                match field.bytes().await {
                    Ok(data) => {
                        let mut temp = match NamedTempFile::new() {
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
                        if let Err(e) = temp.write_all(&data) {
                            return (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                Json(ErrorResponse {
                                    error: format!("Failed to write temp file: {}", e),
                                }),
                            )
                                .into_response();
                        }
                        old_file = Some(temp);
                    }
                    Err(e) => {
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(ErrorResponse {
                                error: format!("Failed to read 'old' file: {}", e),
                            }),
                        )
                            .into_response();
                    }
                }
            }
            "new" => {
                match field.bytes().await {
                    Ok(data) => {
                        let mut temp = match NamedTempFile::new() {
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
                        if let Err(e) = temp.write_all(&data) {
                            return (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                Json(ErrorResponse {
                                    error: format!("Failed to write temp file: {}", e),
                                }),
                            )
                                .into_response();
                        }
                        new_file = Some(temp);
                    }
                    Err(e) => {
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(ErrorResponse {
                                error: format!("Failed to read 'new' file: {}", e),
                            }),
                        )
                            .into_response();
                    }
                }
            }
            "key" => {
                if let Ok(text) = field.text().await {
                    if !text.is_empty() {
                        key = Some(text);
                    }
                }
            }
            "threshold" => {
                if let Ok(text) = field.text().await {
                    if let Ok(val) = text.parse::<f64>() {
                        if val > 0.0 && val <= 1.0 {
                            threshold = val;
                        }
                    }
                }
            }
            "tolerance" => {
                if let Ok(text) = field.text().await {
                    if let Ok(val) = text.parse::<f64>() {
                        if val >= 0.0 {
                            tolerance = val;
                        }
                    }
                }
            }
            "delimiter" => {
                if let Ok(text) = field.text().await {
                    delimiter = parse_delimiter(&text);
                }
            }
            _ => {
                // Ignore unknown fields
            }
        }
    }

    // Validate required files
    let old_temp = match old_file {
        Some(f) => f,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: "Missing required field: 'old' (CSV file)".to_string(),
                }),
            )
                .into_response();
        }
    };

    let new_temp = match new_file {
        Some(f) => f,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: "Missing required field: 'new' (CSV file)".to_string(),
                }),
            )
                .into_response();
        }
    };

    // Build args for orchestrator
    let args = Args::new(
        PathBuf::from(old_temp.path()),
        PathBuf::from(new_temp.path()),
        key,
        threshold,
        tolerance,
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
