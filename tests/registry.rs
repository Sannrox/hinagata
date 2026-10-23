//! The model registry over the API (#1): models and versions stay in the
//! caller's namespace on both surfaces, versions never change, and stage
//! changes are recorded with their actor.

use hinagata::api::proto::hinagata_client::HinagataClient;
use hinagata::api::proto::{
    CreateModelRequest, GetVersionRequest, ListModelsRequest, RegisterVersionRequest,
    SetVersionStageRequest, Signature, TensorSpec,
};
use hinagata::api::{Api, serve};
use hinagata::auth::Jwt;
use hinagata::registry::Registry;
use jsonwebtoken::{EncodingKey, Header, encode};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const SECRET: &[u8] = b"test-only-hmac-secret-0123456789";
const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn jwks() -> String {
    use base64::Engine as _;
    let k = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(SECRET);
    format!(r#"{{"keys":[{{"kty":"oct","kid":"k1","alg":"HS256","k":"{k}"}}]}}"#)
}

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

async fn server() -> (String, tokio::sync::oneshot::Sender<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let auth = Arc::new(Jwt::new(&jwks(), "hinagata", None, "tenant").unwrap());
    let registry = Arc::new(Registry::open_in_memory().unwrap());
    tokio::spawn(serve(Api::new(auth, registry), listener, None, async {
        let _ = stopped.await;
    }));
    (address, stop)
}

fn authed<T>(tenant: &str, message: T) -> tonic::Request<T> {
    let mut request = tonic::Request::new(message);
    request.metadata_mut().insert(
        "authorization",
        format!("Bearer {}", token(tenant)).parse().unwrap(),
    );
    request
}

/// One HTTP/1.1 request as `tenant`; the status code and JSON body.
async fn http(
    address: &str,
    tenant: &str,
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> (u16, serde_json::Value) {
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let body = body.map(|body| body.to_string()).unwrap_or_default();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nhost: hinagata\r\nauthorization: Bearer {}\r\n\
         content-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        token(tenant),
        body.len()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    let status = response[9..12].parse().unwrap();
    let body = response.split("\r\n\r\n").nth(1).unwrap_or("");
    (status, serde_json::from_str(body).unwrap_or_default())
}

fn version_request() -> RegisterVersionRequest {
    RegisterVersionRequest {
        model: "churn".into(),
        artifact_digest: DIGEST.into(),
        framework: "lightgbm".into(),
        format: "onnx".into(),
        signature: Some(Signature {
            inputs: vec![TensorSpec {
                name: "features".into(),
                dtype: "float32".into(),
                shape: vec![-1, 4],
                nullable: false,
            }],
            outputs: vec![TensorSpec {
                name: "score".into(),
                dtype: "float32".into(),
                shape: vec![-1],
                nullable: false,
            }],
        }),
        metrics: [("auc".to_string(), 0.88)].into(),
        tags: [("owner".to_string(), "risk".to_string())].into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn models_and_versions_stay_in_the_callers_namespace_on_both_surfaces() {
    let (address, _stop) = server().await;
    let mut client = HinagataClient::connect(format!("http://{address}"))
        .await
        .unwrap();
    client
        .create_model(authed(
            "tenant-a",
            CreateModelRequest {
                name: "churn".into(),
                description: "monthly churn".into(),
            },
        ))
        .await
        .unwrap();
    let version = client
        .register_version(authed("tenant-a", version_request()))
        .await
        .unwrap()
        .into_inner();
    assert_eq!((version.version, version.stage.as_str()), (1, "candidate"));
    assert_eq!(version.created_by, "svc-tenant-a");

    let (status, body) = http(
        &address,
        "tenant-a",
        "GET",
        "/v1/models/churn/versions/1",
        None,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["artifact_digest"], DIGEST);
    assert_eq!(
        body["signature"]["inputs"][0]["shape"],
        serde_json::json!([-1, 4])
    );

    // Tenant B sees nothing of tenant A's, whatever it puts in the request.
    for path in [
        "/v1/models/churn",
        "/v1/models/churn/versions/1",
        "/v1/models/churn/versions/1?namespace=tenant-a",
    ] {
        let (status, _) = http(&address, "tenant-b", "GET", path, None).await;
        assert_eq!(status, 404, "{path}");
    }
    let (_, listed) = http(&address, "tenant-b", "GET", "/v1/models", None).await;
    assert_eq!(listed["models"], serde_json::json!([]));
    let (_, audit) = http(&address, "tenant-b", "GET", "/v1/audit-events", None).await;
    assert_eq!(audit["events"], serde_json::json!([]));
    let denied = client
        .get_version(authed(
            "tenant-b",
            GetVersionRequest {
                model: "churn".into(),
                version: 1,
            },
        ))
        .await
        .unwrap_err();
    assert_eq!(denied.code(), tonic::Code::NotFound);
    let listed = client
        .list_models(authed("tenant-a", ListModelsRequest::default()))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(listed.models.len(), 1);
}

#[tokio::test]
async fn versions_never_change_and_stage_changes_are_audited() {
    let (address, _stop) = server().await;
    let mut client = HinagataClient::connect(format!("http://{address}"))
        .await
        .unwrap();
    let (status, _) = http(
        &address,
        "tenant-a",
        "POST",
        "/v1/models",
        Some(serde_json::json!({"name": "churn"})),
    )
    .await;
    assert_eq!(status, 200);
    client
        .register_version(authed("tenant-a", version_request()))
        .await
        .unwrap();

    // The same artifact with other metrics is not a new version, and not a
    // change to the stored one.
    let mut changed = version_request();
    changed.metrics.insert("auc".into(), 0.99);
    let refused = client
        .register_version(authed("tenant-a", changed))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), tonic::Code::AlreadyExists);
    let retried = client
        .register_version(authed("tenant-a", version_request()))
        .await
        .unwrap()
        .into_inner();
    assert_eq!((retried.version, retried.metrics["auc"]), (1, 0.88));

    let (status, body) = http(
        &address,
        "tenant-a",
        "POST",
        "/v1/models/churn/versions/1/stage",
        Some(serde_json::json!({"stage": "staging", "expected_stage": "candidate"})),
    )
    .await;
    assert_eq!(
        (status, body["stage"].as_str()),
        (200, Some("staging")),
        "{body}"
    );
    let production = client
        .set_version_stage(authed(
            "tenant-a",
            SetVersionStageRequest {
                model: "churn".into(),
                version: 1,
                stage: "production".into(),
                expected_stage: String::new(),
            },
        ))
        .await
        .unwrap_err();
    assert_eq!(production.code(), tonic::Code::FailedPrecondition);
    assert!(
        production.message().contains("approval hook"),
        "{production:?}"
    );

    let (_, audit) = http(
        &address,
        "tenant-a",
        "GET",
        "/v1/audit-events?model=churn",
        None,
    )
    .await;
    let events = audit["events"].as_array().unwrap();
    let actions: Vec<_> = events
        .iter()
        .map(|event| event["action"].as_str().unwrap())
        .collect();
    assert_eq!(
        actions,
        [
            "model.created",
            "version.registered",
            "version.stage_changed"
        ]
    );
    let stage_change = &events[2];
    assert_eq!(stage_change["actor"], "svc-tenant-a");
    let before: serde_json::Value =
        serde_json::from_str(stage_change["before"].as_str().unwrap()).unwrap();
    let after: serde_json::Value =
        serde_json::from_str(stage_change["after"].as_str().unwrap()).unwrap();
    assert_eq!(before["stage"], "candidate");
    assert_eq!(after["stage"], "staging");
    assert_eq!(after["metrics"]["auc"], 0.88);
}
