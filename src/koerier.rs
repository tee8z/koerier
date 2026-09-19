use std::collections::BTreeMap;
use std::fs;
use std::io::Cursor;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use axum::extract::rejection::PathRejection;
use axum::extract::{Path as RoutePath, RawQuery, State};
use axum::middleware;
use axum::routing::get;
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use clap::Parser;
use image::ImageFormat;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use url::Url;

use crate::error::LnurlError;
use crate::lnd::{Lnd, Node};
mod error;
mod lnd;
#[cfg(test)]
mod tests;

#[derive(Parser)]
#[command(name = "koerier", about = "A Lightning Address server for named LND nodes")]
struct Cli {
    #[arg(long, short = 'c', help = "Path to the TOML configuration file")]
    config: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    koerier: Koerier,
    nodes: BTreeMap<String, Lnd>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Koerier {
    bind_address: SocketAddr,
    domain: String,
    description: String,
    image_path: Option<PathBuf>,
    #[serde(default = "default_timeout")]
    request_timeout_secs: u64,
    #[serde(default = "default_concurrency")]
    max_in_flight: usize,
}
fn default_timeout() -> u64 {
    10
}
fn default_concurrency() -> usize {
    16
}

struct AppState {
    nodes: BTreeMap<String, Node>,
    in_flight: Semaphore,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PayParams {
    metadata: String,
    tag: &'static str,
    min_sendable: u64,
    max_sendable: u64,
    callback: String,
}

#[derive(Serialize)]
struct PaymentRequest {
    pr: String,
    routes: Vec<String>,
}

fn node<'a>(state: &'a AppState, name: &str) -> Result<&'a Node, LnurlError> {
    state
        .nodes
        .get(name)
        .ok_or_else(|| LnurlError::new("Unknown Lightning Address"))
}

async fn return_params(
    State(state): State<Arc<AppState>>,
    user: Result<RoutePath<String>, PathRejection>,
) -> Result<Json<PayParams>, LnurlError> {
    let RoutePath(user) = user.map_err(|_| LnurlError::new("Invalid Lightning Address"))?;
    let node = node(&state, &user)?;
    Ok(Json(PayParams {
        metadata: node.metadata.clone(),
        tag: "payRequest",
        min_sendable: node.min_msat,
        max_sendable: node.max_msat,
        callback: node.callback.clone(),
    }))
}

fn parse_amount(query: Option<&str>) -> Result<u64, LnurlError> {
    let mut amount = None;
    for (key, value) in url::form_urlencoded::parse(query.unwrap_or_default().as_bytes()) {
        if key != "amount" {
            return Err(LnurlError::new("Only the amount parameter is supported"));
        }
        if amount.is_some() {
            return Err(LnurlError::new("Duplicate amount parameter"));
        }
        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(LnurlError::new("Amount must be an integer in millisatoshis"));
        }
        amount = Some(value.parse::<u64>().map_err(|_| LnurlError::new("Invalid amount"))?);
    }
    amount.ok_or_else(|| LnurlError::new("Missing amount parameter"))
}

async fn fetch_invoice(
    State(state): State<Arc<AppState>>,
    user: Result<RoutePath<String>, PathRejection>,
    RawQuery(query): RawQuery,
) -> Result<Json<PaymentRequest>, LnurlError> {
    let RoutePath(user) = user.map_err(|_| LnurlError::new("Invalid Lightning Address"))?;
    let node = node(&state, &user)?;
    let amount = parse_amount(query.as_deref())?;
    if !(node.min_msat..=node.max_msat).contains(&amount) {
        return Err(LnurlError::new("Amount is outside the advertised range"));
    }
    // No wait queue: reject new work when all backend request permits are held.
    let _permit = state
        .in_flight
        .try_acquire()
        .map_err(|_| LnurlError::new("Service is busy; retry later"))?;
    let invoice = node.invoice(amount).await.map_err(|_| {
        // Backend errors can contain credentials and internal URLs; keep them private.
        warn!(node = user, "LND invoice request failed");
        LnurlError::new("Failed to fetch invoice from LND")
    })?;
    Ok(Json(PaymentRequest {
        pr: invoice,
        routes: vec![],
    }))
}

fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/.well-known/lnurlp/{user}", get(return_params))
        .route(
            "/lnurlp/{user}/callback",
            get(fetch_invoice).head(|| async { LnurlError::new("Only GET is supported") }),
        )
        // Liveness only: no LND, chain, or channel readiness check.
        .route("/healthz", get(|| async { Json(serde_json::json!({"status": "ok"})) }))
        .fallback(|| async { LnurlError::new("Unknown endpoint") })
        .method_not_allowed_fallback(|| async { LnurlError::new("Only GET is supported") })
        .layer(middleware::map_response(
            |mut response: axum::response::Response| async move {
                response.headers_mut().insert(
                    axum::http::header::CACHE_CONTROL,
                    axum::http::HeaderValue::from_static("no-store"),
                );
                response
            },
        ))
        .with_state(state)
}

fn public_origin(domain: &str) -> Result<Url> {
    let url = Url::parse(domain).context("invalid koerier.domain URL")?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || domain.contains('@')
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("koerier.domain must be an HTTPS origin without credentials, path, query, or fragment");
    }
    Ok(url)
}

fn valid_alias(alias: &str) -> bool {
    (1..=64).contains(&alias.len())
        && alias
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_')
}

fn image_metadata(image_path: &Path) -> Result<String> {
    let image = image::open(image_path).context("cannot read metadata image")?;
    let mut bytes = Cursor::new(Vec::new());
    image
        .write_to(&mut bytes, ImageFormat::Png)
        .context("cannot encode metadata image")?;
    Ok(STANDARD.encode(bytes.into_inner()))
}

fn build_state(config: Config, config_dir: &Path, credentials_dir: &Path) -> Result<(SocketAddr, Arc<AppState>)> {
    let settings = config.koerier;
    let origin = public_origin(&settings.domain)?;
    if config.nodes.is_empty() {
        bail!("at least one named node is required");
    }
    if settings.request_timeout_secs == 0
        || settings.max_in_flight == 0
        || settings.max_in_flight > Semaphore::MAX_PERMITS
    {
        bail!("request_timeout_secs and max_in_flight must be positive and supported");
    }
    let image = settings
        .image_path
        .as_ref()
        .map(|path| image_metadata(&config_dir.join(path)))
        .transpose()?;
    let mut nodes = BTreeMap::new();
    for (name, lnd) in config.nodes {
        if !valid_alias(&name) {
            bail!("node aliases must contain 1-64 lowercase ASCII letters, digits, hyphens, or underscores");
        }
        let mut metadata = vec![
            ["text/plain".to_owned(), settings.description.clone()],
            [
                "text/identifier".to_owned(),
                format!("{}@{}", name, origin.host_str().expect("validated host")),
            ],
        ];
        if let Some(image) = &image {
            metadata.push(["image/png;base64".to_owned(), image.clone()]);
        }
        let metadata = serde_json::to_string(&metadata)?;
        let callback = origin.join(&format!("/lnurlp/{name}/callback"))?.to_string();
        let node = Node::new(
            lnd,
            credentials_dir,
            metadata,
            callback,
            Duration::from_secs(settings.request_timeout_secs),
        )
        .with_context(|| format!("invalid configuration for node {name}"))?;
        nodes.insert(name, node);
    }
    Ok((
        settings.bind_address,
        Arc::new(AppState {
            nodes,
            in_flight: Semaphore::new(settings.max_in_flight),
        }),
    ))
}

fn load_config(path: &Path) -> Result<(SocketAddr, Arc<AppState>)> {
    let config = fs::read_to_string(path).context("cannot read configuration file")?;
    let config: Config = toml::from_str(&config).context("invalid TOML configuration")?;
    let config_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let credentials_dir = std::env::var_os("CREDENTIALS_DIRECTORY")
        .map(PathBuf::from)
        .unwrap_or_else(|| config_dir.to_path_buf());
    build_state(config, config_dir, &credentials_dir)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();
    let args = Cli::parse();
    let (address, state) = load_config(&args.config)?;
    let listener = TcpListener::bind(address).await.context("cannot bind HTTP listener")?;
    info!(address = %listener.local_addr()?, nodes = state.nodes.len(), "koerier is listening");
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

async fn shutdown() {
    #[cfg(unix)]
    {
        if let Ok(mut terminate) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}
