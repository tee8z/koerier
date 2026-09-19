use super::*;
use axum::body::{Body, to_bytes};
use axum::http::Request;
use rcgen::generate_simple_self_signed;
use rustls::pki_types::PrivatePkcs8KeyDer;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;
use tower::ServiceExt;

struct CapturedRequest {
    headers: String,
    body: Value,
}

/// A real HTTPS peer exercises the production client and certificate checks.
struct FakeLnd {
    dir: TempDir,
    address: SocketAddr,
    requests: mpsc::UnboundedReceiver<CapturedRequest>,
    task: JoinHandle<()>,
}

impl Drop for FakeLnd {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeLnd {
    async fn new(status: &str, body: String, delay: Duration, valid_host: bool) -> Self {
        let names = if valid_host {
            vec!["localhost".into(), "127.0.0.1".into()]
        } else {
            vec!["wrong.example".into()]
        };
        let cert = generate_simple_self_signed(names).unwrap();
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("tls.cert"), cert.cert.pem()).unwrap();
        fs::write(dir.path().join("invoice.macaroon"), [0, 1, 0xfe, 0xff]).unwrap();
        let key = PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der());
        let tls = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert.cert.der().clone()], key.into())
            .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(tls));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, requests) = mpsc::unbounded_channel();
        let reply = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\nLocation: /must-not-follow\r\n\r\n{body}",
            body.len()
        );
        let task = tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                let sender = sender.clone();
                let reply = reply.clone();
                tokio::spawn(async move {
                    let Ok(mut stream) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let mut bytes = Vec::new();
                    let (headers_end, content_length) = loop {
                        let mut buffer = [0u8; 1024];
                        let Ok(count) = stream.read(&mut buffer).await else {
                            return;
                        };
                        if count == 0 {
                            return;
                        }
                        bytes.extend_from_slice(&buffer[..count]);
                        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                            let headers = String::from_utf8_lossy(&bytes[..end]);
                            let length = headers
                                .lines()
                                .find_map(|line| {
                                    let (name, value) = line.split_once(':')?;
                                    name.eq_ignore_ascii_case("content-length")
                                        .then(|| value.trim().parse::<usize>().unwrap())
                                })
                                .unwrap();
                            break (end + 4, length);
                        }
                    };
                    while bytes.len() < headers_end + content_length {
                        let mut buffer = [0u8; 1024];
                        let Ok(count) = stream.read(&mut buffer).await else {
                            return;
                        };
                        if count == 0 {
                            return;
                        }
                        bytes.extend_from_slice(&buffer[..count]);
                    }
                    let request = CapturedRequest {
                        headers: String::from_utf8(bytes[..headers_end].to_vec()).unwrap(),
                        body: serde_json::from_slice(&bytes[headers_end..headers_end + content_length]).unwrap(),
                    };
                    let _ = sender.send(request);
                    tokio::time::sleep(delay).await;
                    let _ = stream.write_all(reply.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        Self {
            dir,
            address,
            requests,
            task,
        }
    }

    async fn ok(invoice: &str) -> Self {
        Self::new(
            "200 OK",
            json!({"payment_request": invoice}).to_string(),
            Duration::ZERO,
            true,
        )
        .await
    }

    fn backend(&self) -> Lnd {
        Lnd {
            rest_host: self.address,
            tls_cert_path: self.dir.path().join("tls.cert"),
            invoice_macaroon_path: self.dir.path().join("invoice.macaroon"),
            min_invoice_amount: 1,
            max_invoice_amount: 1_000_000,
            invoice_expiry_sec: 300,
        }
    }
}

fn config(nodes: BTreeMap<String, Lnd>) -> Config {
    Config {
        koerier: Koerier {
            bind_address: "127.0.0.1:0".parse().unwrap(),
            domain: "https://ln.example".into(),
            description: "Cluster invoices".into(),
            image_path: None,
            request_timeout_secs: 1,
            max_in_flight: 1,
        },
        nodes,
    }
}

fn app(config: Config, path: &Path) -> (Router, Arc<AppState>) {
    let (_, state) = build_state(config, path, path).unwrap();
    (router(state.clone()), state)
}

async fn get(app: &Router, path: &str) -> Value {
    let request = Request::builder()
        .uri(path)
        .header("host", "attacker.example")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert!(response.status().is_success());
    assert_eq!(response.headers()["content-type"], "application/json");
    assert_eq!(response.headers()["cache-control"], "no-store");
    serde_json::from_slice(&to_bytes(response.into_body(), 128 * 1024).await.unwrap()).unwrap()
}

#[tokio::test]
async fn routes_named_nodes_with_exact_msat_and_metadata_hash() {
    let mut odin = FakeLnd::ok("invoice-odin").await;
    let mut thor = FakeLnd::ok("invoice-thor").await;
    let image_path = odin.dir.path().join("icon.png");
    image::RgbaImage::from_pixel(1, 1, image::Rgba([12, 34, 56, 255]))
        .save(&image_path)
        .unwrap();
    let mut settings = config(BTreeMap::from([
        ("odin".into(), odin.backend()),
        ("thor".into(), thor.backend()),
    ]));
    settings.koerier.image_path = Some("icon.png".into());
    let (app, _) = app(settings, odin.dir.path());
    // Metadata and credentials remain stable even when their source files change.
    fs::remove_file(&image_path).unwrap();
    fs::remove_file(odin.dir.path().join("invoice.macaroon")).unwrap();
    fs::remove_file(odin.dir.path().join("tls.cert")).unwrap();
    for (name, expected_invoice, requests) in [
        ("odin", "invoice-odin", &mut odin.requests),
        ("thor", "invoice-thor", &mut thor.requests),
    ] {
        let params = get(&app, &format!("/.well-known/lnurlp/{name}")).await;
        assert_eq!(params["callback"], format!("https://ln.example/lnurlp/{name}/callback"));
        assert_eq!(params["tag"], "payRequest");
        assert_eq!(params["minSendable"], 1000);
        assert_eq!(params["maxSendable"], 1_000_000_000);
        let metadata = params["metadata"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(metadata).unwrap();
        assert_eq!(parsed[1], json!(["text/identifier", format!("{name}@ln.example")]));
        assert_eq!(parsed[2][0], "image/png;base64");
        let invoice = get(&app, &format!("/lnurlp/{name}/callback?amount=1999")).await;
        assert_eq!(invoice, json!({"pr": expected_invoice, "routes": []}));
        let request = requests.recv().await.unwrap();
        assert!(request.headers.starts_with("POST /v1/invoices HTTP/1.1"));
        assert!(
            request
                .headers
                .to_ascii_lowercase()
                .contains("grpc-metadata-macaroon: 0001feff")
        );
        assert_eq!(request.body["value_msat"], "1999");
        assert_eq!(request.body["expiry"], "300");
        assert_eq!(request.body["private"], true);
        assert!(request.body.get("value").is_none());
        assert_eq!(
            request.body["description_hash"],
            STANDARD.encode(Sha256::digest(metadata.as_bytes()))
        );
    }
}

#[tokio::test]
async fn invalid_requests_do_not_contact_lnd() {
    let mut lnd = FakeLnd::ok("invoice").await;
    let (app, _) = app(config(BTreeMap::from([("odin".into(), lnd.backend())])), lnd.dir.path());
    for path in [
        "/.well-known/lnurlp/unknown",
        "/.well-known/lnurlp/Odin",
        "/lnurlp/unknown/callback?amount=1000",
        "/lnurlp/odin/callback",
        "/lnurlp/odin/callback?amount=",
        "/lnurlp/odin/callback?amount=-1",
        "/lnurlp/odin/callback?amount=1.5",
        "/lnurlp/odin/callback?amount=1000&amount=1000",
        "/lnurlp/odin/callback?amount=1000&%61mount=2000",
        "/lnurlp/odin/callback?amount=18446744073709551616",
        "/lnurlp/odin/callback?amount=999",
        "/lnurlp/odin/callback?amount=1000000001",
        "/lnurlp/odin/callback?amount=1000&node=thor",
        "/lnurlp/callback?amount=1000",
        "/.well-known/lnurlp/%FF",
    ] {
        assert_eq!(get(&app, path).await["status"], "ERROR", "{path}");
    }
    assert!(lnd.requests.try_recv().is_err());
    let head = Request::builder()
        .method("HEAD")
        .uri("/lnurlp/odin/callback?amount=1000")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(head).await.unwrap();
    assert!(to_bytes(response.into_body(), 1024).await.unwrap().is_empty());
    assert!(lnd.requests.try_recv().is_err(), "HEAD must not create an invoice");
}

#[tokio::test]
async fn overload_is_rejected_without_queuing_and_liveness_stays_available() {
    let mut lnd = FakeLnd::ok("invoice").await;
    let (app, state) = app(config(BTreeMap::from([("odin".into(), lnd.backend())])), lnd.dir.path());
    let permit = state.in_flight.try_acquire().unwrap();
    let response = get(&app, "/lnurlp/odin/callback?amount=1000").await;
    assert_eq!(response["reason"], "Service is busy; retry later");
    assert_eq!(get(&app, "/healthz").await, json!({"status": "ok"}));
    assert_eq!(get(&app, "/.well-known/lnurlp/odin").await["tag"], "payRequest");
    assert!(lnd.requests.try_recv().is_err());
    drop(permit);
    assert_eq!(get(&app, "/lnurlp/odin/callback?amount=1000").await["pr"], "invoice");
}

#[tokio::test]
async fn backend_errors_are_sanitized_and_release_permits() {
    for (status, body) in [
        (
            "500 Internal Server Error",
            json!({"payment_request": "secret"}).to_string(),
        ),
        ("200 OK", json!({"payment_request": 123}).to_string()),
        ("200 OK", json!({"payment_request": ""}).to_string()),
        ("200 OK", json!({"payment_request": "  "}).to_string()),
        ("200 OK", "{".into()),
        ("200 OK", json!({"payment_request": "x".repeat(70 * 1024)}).to_string()),
        ("302 Found", json!({"payment_request": "secret"}).to_string()),
    ] {
        let mut lnd = FakeLnd::new(status, body, Duration::ZERO, true).await;
        let (app, state) = app(config(BTreeMap::from([("odin".into(), lnd.backend())])), lnd.dir.path());
        let response = get(&app, "/lnurlp/odin/callback?amount=1000").await;
        assert_eq!(
            response,
            json!({"status": "ERROR", "reason": "Failed to fetch invoice from LND"})
        );
        assert_eq!(state.in_flight.available_permits(), 1);
        let _ = lnd.requests.recv().await.unwrap();
        assert!(lnd.requests.try_recv().is_err(), "redirect must not be followed");
    }
}

#[tokio::test]
async fn tls_hostname_is_verified() {
    let mut lnd = FakeLnd::new(
        "200 OK",
        json!({"payment_request": "invoice"}).to_string(),
        Duration::ZERO,
        false,
    )
    .await;
    let (app, _) = app(config(BTreeMap::from([("odin".into(), lnd.backend())])), lnd.dir.path());
    assert_eq!(get(&app, "/lnurlp/odin/callback?amount=1000").await["status"], "ERROR");
    assert!(lnd.requests.try_recv().is_err());
}

#[tokio::test]
async fn untrusted_tls_certificate_is_rejected() {
    let mut lnd = FakeLnd::ok("invoice").await;
    let other = FakeLnd::ok("other").await;
    let mut backend = lnd.backend();
    backend.tls_cert_path = other.dir.path().join("tls.cert");
    let (app, _) = app(config(BTreeMap::from([("odin".into(), backend)])), lnd.dir.path());
    assert_eq!(get(&app, "/lnurlp/odin/callback?amount=1000").await["status"], "ERROR");
    assert!(lnd.requests.try_recv().is_err());
}

#[tokio::test]
async fn stalled_lnd_times_out() {
    let lnd = FakeLnd::new(
        "200 OK",
        json!({"payment_request": "invoice"}).to_string(),
        Duration::from_secs(3),
        true,
    )
    .await;
    let (app, state) = app(config(BTreeMap::from([("odin".into(), lnd.backend())])), lnd.dir.path());
    let result = tokio::time::timeout(Duration::from_secs(2), get(&app, "/lnurlp/odin/callback?amount=1000"))
        .await
        .unwrap();
    assert_eq!(result["status"], "ERROR");
    assert_eq!(state.in_flight.available_permits(), 1);
}

#[test]
fn config_rejects_unsafe_origins_and_aliases() {
    for origin in [
        "http://ln.example",
        "https://u:p@ln.example",
        "https://@ln.example",
        "https://ln.example/path",
        "https://ln.example?query",
        "https://ln.example#fragment",
        "file:///tmp",
    ] {
        assert!(public_origin(origin).is_err(), "{origin}");
    }
    assert!(public_origin("https://ln.example/").is_ok());
    for alias in ["", "Odin", "a/b", "../", "white space", "å"] {
        assert!(!valid_alias(alias));
    }
    assert!(valid_alias("node-1_test"));
    assert!(!valid_alias(&"a".repeat(65)));
}

#[tokio::test]
async fn config_validates_limits_and_resolves_credential_directory() {
    let lnd = FakeLnd::ok("invoice").await;
    let separate_config_dir = tempfile::tempdir().unwrap();
    let mut backend = lnd.backend();
    backend.tls_cert_path = "tls.cert".into();
    backend.invoice_macaroon_path = "invoice.macaroon".into();
    let settings = config(BTreeMap::from([("odin".into(), backend)]));
    assert!(build_state(settings, separate_config_dir.path(), lnd.dir.path()).is_ok());
    for (min, max, expiry) in [(0, 1, 300), (2, 1, 300), (1, u64::MAX, 300), (1, 100, 0)] {
        let mut backend = lnd.backend();
        backend.min_invoice_amount = min;
        backend.max_invoice_amount = max;
        backend.invoice_expiry_sec = expiry;
        assert!(
            build_state(
                config(BTreeMap::from([("odin".into(), backend)])),
                lnd.dir.path(),
                lnd.dir.path()
            )
            .is_err()
        );
    }
    for (timeout, permits) in [(0, 1), (1, 0), (1, usize::MAX)] {
        let mut settings = config(BTreeMap::from([("odin".into(), lnd.backend())]));
        settings.koerier.request_timeout_secs = timeout;
        settings.koerier.max_in_flight = permits;
        assert!(build_state(settings, lnd.dir.path(), lnd.dir.path()).is_err());
    }
    assert!(build_state(config(BTreeMap::new()), lnd.dir.path(), lnd.dir.path()).is_err());
}

#[test]
fn toml_defaults_and_unknown_fields_are_checked() {
    let text = r#"
[koerier]
domain = "https://ln.example"
bind_address = "127.0.0.1:3441"
description = "test"
[nodes.odin]
rest_host = "127.0.0.1:8080"
tls_cert_path = "tls.cert"
invoice_macaroon_path = "invoice.macaroon"
min_invoice_amount = 1
max_invoice_amount = 1000000
invoice_expiry_sec = 300
"#;
    let parsed: Config = toml::from_str(text).unwrap();
    assert_eq!(parsed.koerier.request_timeout_secs, 10);
    assert_eq!(parsed.koerier.max_in_flight, 16);
    assert!(toml::from_str::<Config>(&format!("{text}\nunknown = true")).is_err());
}

#[test]
fn startup_installs_crypto_provider() {
    const CHILD_ENV: &str = "KOERIER_PROVIDER_TEST_CHILD";
    if std::env::var_os(CHILD_ENV).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tests::startup_installs_crypto_provider", "--nocapture"])
            .env(CHILD_ENV, "1")
            .status()
            .unwrap();
        assert!(
            status.success(),
            "fresh process must construct the production TLS client"
        );
        return;
    }
    assert!(rustls::crypto::CryptoProvider::get_default().is_none());
    let cert = generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("tls.cert"), cert.cert.pem()).unwrap();
    fs::write(dir.path().join("invoice.macaroon"), [1, 2, 3]).unwrap();
    let backend = Lnd {
        rest_host: "127.0.0.1:8080".parse().unwrap(),
        tls_cert_path: "tls.cert".into(),
        invoice_macaroon_path: "invoice.macaroon".into(),
        min_invoice_amount: 1,
        max_invoice_amount: 100,
        invoice_expiry_sec: 300,
    };
    // A runtime is required by the asynchronous client, but no fake TLS server has run.
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let _guard = runtime.enter();
    assert!(
        build_state(
            config(BTreeMap::from([("odin".into(), backend)])),
            dir.path(),
            dir.path()
        )
        .is_ok()
    );
}
