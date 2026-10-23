//! Lineage (#4): a version registered from a training run receipt shows its
//! inputs, and the dataset shows the versions trained on it, on both
//! surfaces and only within the caller's namespace.

use hinagata::api::proto::hinagata_client::HinagataClient;
use hinagata::api::proto::{
    CodeReference, CreateModelRequest, Lineage, ListVersionsTrainedOnRequest,
    RegisterVersionRequest,
};
use hinagata::api::{Api, serve};
use hinagata::auth::Jwt;
use hinagata::registry::Registry;
use jsonwebtoken::{EncodingKey, Header, encode};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const SECRET: &[u8] = b"test-only-hmac-secret-0123456789";
/// A synthetic batch runner receipt (schema 6) for a training run.
const RECEIPT: &str = include_str!("fixtures/training-run-receipt.json");
const MODEL: &[u8] = b"churn model weights v1";
const MODEL_DIGEST: &str =
    "sha256:01e362ecda396b3bac2c157f468fa271b1282a836bd7d67e7b0110308c2972a5";
const ORDERS: &str = "sha256:2222222222222222222222222222222222222222222222222222222222222222";

fn token(tenant: &str) -> String {
    let mut header = Header::new(jsonwebtoken::Algorithm::HS256);
    header.kid = Some("k1".into());
    let claims = serde_json::json!({
        "sub": format!("svc-{tenant}"),
        "aud": "hinagata",
        "tenant": tenant,
        "exp": 4_102_444_800_u64,
    });
    encode(&header, &claims, &EncodingKey::from_secret(SECRET)).unwrap()
}

async fn server() -> String {
    use base64::Engine as _;
    let k = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(SECRET);
    let jwks = format!(r#"{{"keys":[{{"kty":"oct","kid":"k1","alg":"HS256","k":"{k}"}}]}}"#);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let auth = Arc::new(Jwt::new(&jwks, "hinagata", None, "tenant").unwrap());
    let registry = Arc::new(Registry::open_in_memory().unwrap());
    tokio::spawn(serve(
        Api::new(
            auth,
            registry,
            Arc::new(hinagata::artifacts::ArtifactStore::new(
                Arc::new(object_store::memory::InMemory::new()),
                1 << 20,
            )),
        ),
        listener,
        None,
        async { std::future::pending::<()>().await },
    ));
    address
}

fn authed<T>(tenant: &str, message: T) -> tonic::Request<T> {
    let mut request = tonic::Request::new(message);
    request.metadata_mut().insert(
        "authorization",
        format!("Bearer {}", token(tenant)).parse().unwrap(),
    );
    request
}

async fn get(address: &str, tenant: &str, path: &str) -> (u16, serde_json::Value) {
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let request = format!(
        "GET {path} HTTP/1.1\r\nhost: hinagata\r\nauthorization: Bearer {}\r\nconnection: close\r\n\r\n",
        token(tenant)
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    let status = response[9..12].parse().unwrap();
    let body = response.split("\r\n\r\n").nth(1).unwrap_or("");
    (status, serde_json::from_str(body).unwrap_or_default())
}

/// Upload `bytes` as an artifact of `tenant` and return its digest.
async fn upload(address: &str, tenant: &str, bytes: &[u8]) -> String {
    use sha2::Digest as _;
    let mut client = HinagataClient::connect(format!("http://{address}"))
        .await
        .unwrap();
    let digest = format!("sha256:{:x}", sha2::Sha256::digest(bytes));
    let begun = client
        .begin_artifact_upload(authed(
            tenant,
            hinagata::api::proto::BeginArtifactUploadRequest {
                digest: digest.clone(),
                size_bytes: bytes.len() as u64,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    client
        .upload_artifact_chunk(authed(
            tenant,
            hinagata::api::proto::UploadArtifactChunkRequest {
                upload_id: begun.upload_id.clone(),
                offset: 0,
                data: bytes.to_vec(),
            },
        ))
        .await
        .unwrap();
    client
        .complete_artifact_upload(authed(
            tenant,
            hinagata::api::proto::CompleteArtifactUploadRequest {
                upload_id: begun.upload_id,
            },
        ))
        .await
        .unwrap();
    digest
}

fn from_receipt(artifact_digest: &str) -> RegisterVersionRequest {
    RegisterVersionRequest {
        model: "churn".into(),
        artifact_digest: artifact_digest.into(),
        framework: "lightgbm".into(),
        format: "onnx".into(),
        metrics: [("auc".to_string(), 0.87)].into(),
        lineage: Some(Lineage {
            code: Some(CodeReference {
                repository: "github.com/example/churn-model".into(),
                commit: "9fceb02d0ae598e95dc970b74767f19372d61af8".into(),
                digest: String::new(),
            }),
            parameters: [("num_leaves".to_string(), "31".to_string())].into(),
            ..Default::default()
        }),
        training_run_receipt_json: RECEIPT.into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn a_version_registered_from_a_training_receipt_shows_lineage_both_ways() {
    let address = server().await;
    let mut client = HinagataClient::connect(format!("http://{address}"))
        .await
        .unwrap();
    client
        .create_model(authed(
            "tenant-a",
            CreateModelRequest {
                name: "churn".into(),
                description: String::new(),
            },
        ))
        .await
        .unwrap();
    assert_eq!(upload(&address, "tenant-a", MODEL).await, MODEL_DIGEST);
    let version = client
        .register_version(authed("tenant-a", from_receipt(MODEL_DIGEST)))
        .await
        .unwrap()
        .into_inner();
    let lineage = version.lineage.unwrap();
    assert_eq!(lineage.run_receipt_id, "train-churn-2026-10-22");
    assert!(lineage.run_receipt_digest.starts_with("sha256:"));
    let datasets: Vec<_> = lineage
        .training_datasets
        .iter()
        .map(|dataset| (dataset.dataset_id.as_str(), &dataset.digest[..9]))
        .collect();
    assert_eq!(
        datasets,
        [("customers", "sha256:11"), ("orders", "sha256:22")]
    );

    // The inputs of the version, over HTTP.
    let (status, body) = get(&address, "tenant-a", "/v1/models/churn/versions/1").await;
    assert_eq!(status, 200, "{body}");
    let lineage = &body["lineage"];
    assert_eq!(
        lineage["code"]["commit"],
        "9fceb02d0ae598e95dc970b74767f19372d61af8"
    );
    assert_eq!(
        lineage["code"]["digest"],
        "sha256:3333333333333333333333333333333333333333333333333333333333333333"
    );
    assert_eq!(lineage["parameters"]["num_leaves"], "31");
    assert_eq!(lineage["training_datasets"][1]["digest"], ORDERS);

    // The models trained on a dataset version, over gRPC and HTTP.
    let trained = client
        .list_versions_trained_on(authed(
            "tenant-a",
            ListVersionsTrainedOnRequest {
                dataset_id: "orders".into(),
                digest: ORDERS.into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(trained.versions.len(), 1);
    assert_eq!(
        (
            trained.versions[0].model.as_str(),
            trained.versions[0].version
        ),
        ("churn", 1)
    );
    let (_, body) = get(
        &address,
        "tenant-a",
        "/v1/lineage/trained-on?dataset_id=customers",
    )
    .await;
    assert_eq!(body["versions"][0]["artifact_digest"], MODEL_DIGEST);
    let other_digest = format!(
        "/v1/lineage/trained-on?dataset_id=orders&digest=sha256:{}",
        "9".repeat(64)
    );
    let (_, body) = get(&address, "tenant-a", &other_digest).await;
    assert_eq!(body["versions"], serde_json::json!([]));
    let (_, body) = get(
        &address,
        "tenant-b",
        "/v1/lineage/trained-on?dataset_id=orders",
    )
    .await;
    assert_eq!(body["versions"], serde_json::json!([]));

    // A model the run did not produce cannot claim its lineage.
    let other = upload(&address, "tenant-a", b"another model").await;
    let refused = client
        .register_version(authed("tenant-a", from_receipt(&other)))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), tonic::Code::InvalidArgument);
    assert!(refused.message().contains("did not produce"), "{refused:?}");
}
