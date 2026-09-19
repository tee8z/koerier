use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::{Certificate, Client};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_SAFE_JSON_INTEGER: u64 = (1_u64 << 53) - 1;

/// Configuration for one named LND backend. Bounds are in satoshis.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Lnd {
    pub(crate) rest_host: SocketAddr,
    pub(crate) tls_cert_path: PathBuf,
    pub(crate) invoice_macaroon_path: PathBuf,
    pub(crate) min_invoice_amount: u64,
    pub(crate) max_invoice_amount: u64,
    pub(crate) invoice_expiry_sec: u32,
}

/// Immutable runtime state, with credentials and metadata read once at startup.
pub(crate) struct Node {
    client: Client,
    invoice_url: String,
    expiry: String,
    description_hash: String,
    pub(crate) metadata: String,
    pub(crate) callback: String,
    pub(crate) min_msat: u64,
    pub(crate) max_msat: u64,
}

impl Node {
    pub(crate) fn new(
        config: Lnd,
        credentials_dir: &Path,
        metadata: String,
        callback: String,
        timeout: Duration,
    ) -> Result<Self> {
        let min_msat = config
            .min_invoice_amount
            .checked_mul(1000)
            .context("minimum amount overflows msat")?;
        let max_msat = config
            .max_invoice_amount
            .checked_mul(1000)
            .context("maximum amount overflows msat")?;
        if min_msat == 0 || min_msat > max_msat || max_msat > MAX_SAFE_JSON_INTEGER {
            bail!("invoice bounds must be positive, ordered, and fit an exact JSON integer");
        }
        if config.invoice_expiry_sec == 0 {
            bail!("invoice_expiry_sec must be positive");
        }
        let certificate =
            fs::read(credentials_dir.join(config.tls_cert_path)).context("cannot read LND TLS certificate")?;
        let certificate = Certificate::from_pem(&certificate).context("invalid LND TLS certificate")?;
        let macaroon =
            fs::read(credentials_dir.join(config.invoice_macaroon_path)).context("cannot read LND invoice macaroon")?;
        if macaroon.is_empty() {
            bail!("LND invoice macaroon must not be empty");
        }
        let mut header = HeaderValue::from_str(&hex::encode(macaroon))?;
        header.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert("Grpc-Metadata-macaroon", header);
        // LND serves a self-signed CA:true certificate. Native TLS accepts that
        // certificate as its configured trust anchor while still verifying the
        // certificate chain, validity period, and requested host/IP address.
        let client = Client::builder()
            .tls_backend_native()
            .tls_certs_only([certificate])
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .timeout(timeout)
            .connect_timeout(timeout)
            .build()
            .context("cannot build LND HTTPS client")?;
        Ok(Self {
            client,
            invoice_url: format!("https://{}/v1/invoices", config.rest_host),
            expiry: config.invoice_expiry_sec.to_string(),
            description_hash: STANDARD.encode(Sha256::digest(metadata.as_bytes())),
            metadata,
            callback,
            min_msat,
            max_msat,
        })
    }

    /// Request an ordinary invoice without rounding millisatoshis to satoshis.
    pub(crate) async fn invoice(&self, amount_msat: u64) -> Result<String> {
        let mut response = self
            .client
            .post(&self.invoice_url)
            .json(&json!({
                "value_msat": amount_msat.to_string(),
                "description_hash": self.description_hash,
                "expiry": self.expiry,
                "private": true,
            }))
            .send()
            .await?
            .error_for_status()?;
        if !response.status().is_success() {
            bail!("unexpected LND response status");
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            bail!("LND response exceeds limit");
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                bail!("LND response exceeds limit");
            }
            body.extend_from_slice(&chunk);
        }
        #[derive(Deserialize)]
        struct InvoiceResponse {
            payment_request: String,
        }
        let invoice: InvoiceResponse = serde_json::from_slice(&body)?;
        if invoice.payment_request.trim().is_empty() {
            bail!("LND returned an empty payment request");
        }
        Ok(invoice.payment_request)
    }
}
