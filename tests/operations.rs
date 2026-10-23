//! Operations (#9): health reports whether the server can do work, metrics
//! are Prometheus text, and the diagnostics bundle carries no secrets.

use hinagata::api::{Api, Setup, serve};
use hinagata::artifacts::ArtifactStore;
use hinagata::auth::StaticToken;
use hinagata::registry::Registry;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const TOKEN: &str = "operations-test-token";

async fn server(store: Arc<dyn object_store::ObjectStore>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let api = Api::new(
        Arc::new(StaticToken::new(TOKEN, "tenant-a")),
        Arc::new(Registry::open_in_memory().unwrap()),
        Arc::new(ArtifactStore::new(store, 1 << 20)),
    )
    .with_setup(Setup {
        auth: "static_token".into(),
        artifact_store: "local".into(),
        tls: false,
        mutual_tls: false,
    });
    tokio::spawn(serve(api, listener, None, async {
        std::future::pending::<()>().await
    }));
    address
}

async fn get(address: &str, path: &str, token: Option<&str>) -> (u16, String) {
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let auth = token
        .map(|token| format!("authorization: Bearer {token}\r\n"))
        .unwrap_or_default();
    let request =
        format!("GET {path} HTTP/1.1\r\nhost: hinagata\r\n{auth}connection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    let status = response[9..12].parse().unwrap();
    let body = response.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    (status, body)
}

#[tokio::test]
async fn health_metrics_and_diagnostics_report_without_secrets() {
    let address = server(Arc::new(object_store::memory::InMemory::new())).await;
    let (status, body) = get(&address, "/healthz", None).await;
    assert_eq!((status, body.contains("\"ok\"")), (200, true), "{body}");

    let (status, metrics) = get(&address, "/metrics", None).await;
    assert_eq!(status, 200);
    for line in [
        "hinagata_up 1",
        "hinagata_models 0",
        "# TYPE hinagata_versions gauge",
    ] {
        assert!(metrics.contains(line), "{line}: {metrics}");
    }

    // The bundle needs a credential and never repeats it.
    assert_eq!(get(&address, "/v1/diagnostics", None).await.0, 401);
    let (status, body) = get(&address, "/v1/diagnostics", Some(TOKEN)).await;
    assert_eq!(status, 200, "{body}");
    let bundle: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(bundle["setup"]["auth"], "static_token");
    assert_eq!(bundle["healthy"], true);
    assert_eq!(bundle["namespace_totals"]["models"], 0);
    assert!(!body.contains(TOKEN), "{body}");
}

#[tokio::test]
async fn health_fails_when_the_artifact_store_cannot_answer() {
    // An S3 endpoint on a closed loopback port, without retries.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    let store = object_store::aws::AmazonS3Builder::new()
        .with_bucket_name("missing")
        .with_region("us-east-1")
        .with_access_key_id("unused")
        .with_secret_access_key("unused")
        .with_endpoint(endpoint)
        .with_allow_http(true)
        .with_retry(object_store::RetryConfig {
            max_retries: 0,
            retry_timeout: std::time::Duration::from_secs(1),
            ..Default::default()
        })
        .build()
        .unwrap();
    let address = server(Arc::new(store)).await;
    let (status, body) = get(&address, "/healthz", None).await;
    assert_eq!(status, 503, "{body}");
    assert!(body.contains("artifact_store"), "{body}");
    let (_, metrics) = get(&address, "/metrics", None).await;
    assert!(metrics.contains("hinagata_up 0"), "{metrics}");
}
