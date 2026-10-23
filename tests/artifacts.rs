//! Artifact store (#11): the same scenarios on local disk and, when
//! `HINAGATA_TEST_S3_ENDPOINT` and `HINAGATA_TEST_S3_BUCKET` name one (with
//! `AWS_*` credentials), on an S3-compatible store. Set
//! `HINAGATA_REQUIRE_S3=1` to fail instead of skipping without one.

use hinagata::api::proto::hinagata_client::HinagataClient;
use hinagata::api::proto::{
    BeginArtifactUploadRequest, CompleteArtifactUploadRequest, CreateModelRequest,
    ReadArtifactRequest, RegisterVersionRequest, UploadArtifactChunkRequest,
};
use hinagata::api::{Api, serve};
use hinagata::artifacts::ArtifactStore;
use hinagata::auth::Jwt;
use hinagata::registry::Registry;
use jsonwebtoken::{EncodingKey, Header, encode};
use object_store::path::Path as StorePath;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};
use sha2::Digest as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

const SECRET: &[u8] = b"test-only-hmac-secret-0123456789";
const LIMIT: u64 = 64 * 1024;
static SEQ: AtomicU64 = AtomicU64::new(0);

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

fn authed<T>(tenant: &str, message: T) -> tonic::Request<T> {
    let mut request = tonic::Request::new(message);
    request.metadata_mut().insert(
        "authorization",
        format!("Bearer {}", token(tenant)).parse().unwrap(),
    );
    request
}

async fn client(raw: Arc<dyn ObjectStore>) -> HinagataClient<tonic::transport::Channel> {
    use base64::Engine as _;
    let k = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(SECRET);
    let jwks = format!(r#"{{"keys":[{{"kty":"oct","kid":"k1","alg":"HS256","k":"{k}"}}]}}"#);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let auth = Arc::new(Jwt::new(&jwks, "hinagata", None, "tenant").unwrap());
    let registry = Arc::new(Registry::open_in_memory().unwrap());
    let artifacts = Arc::new(ArtifactStore::new(raw, LIMIT));
    tokio::spawn(serve(
        Api::new(auth, registry, artifacts),
        listener,
        None,
        async { std::future::pending::<()>().await },
    ));
    HinagataClient::connect(format!("http://{address}"))
        .await
        .unwrap()
}

fn digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", sha2::Sha256::digest(bytes))
}

async fn read(
    client: &mut HinagataClient<tonic::transport::Channel>,
    tenant: &str,
    digest: &str,
) -> Result<Vec<u8>, tonic::Status> {
    let mut stream = client
        .read_artifact(authed(
            tenant,
            ReadArtifactRequest {
                digest: digest.into(),
            },
        ))
        .await?
        .into_inner();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.message().await? {
        bytes.extend(chunk.data);
    }
    Ok(bytes)
}

async fn scenarios(raw: Arc<dyn ObjectStore>) {
    let mut client = client(raw.clone()).await;
    let body: Vec<u8> = (0..3000u32).map(|i| (i % 251) as u8).collect();
    let id = digest(&body);
    let begin = |size_bytes: u64| BeginArtifactUploadRequest {
        digest: id.clone(),
        size_bytes,
    };
    let chunk = |upload_id: &str, start: usize, end: usize| UploadArtifactChunkRequest {
        upload_id: upload_id.into(),
        offset: start as u64,
        data: body[start..end].to_vec(),
    };

    // An oversized upload is refused.
    let oversized = client
        .begin_artifact_upload(authed("tenant-a", begin(LIMIT + 1)))
        .await
        .unwrap_err();
    assert_eq!(oversized.code(), tonic::Code::ResourceExhausted);

    // An interrupted upload resumes where it stopped.
    let started = client
        .begin_artifact_upload(authed("tenant-a", begin(body.len() as u64)))
        .await
        .unwrap()
        .into_inner();
    client
        .upload_artifact_chunk(authed("tenant-a", chunk(&started.upload_id, 0, 1000)))
        .await
        .unwrap();
    let resumed = client
        .begin_artifact_upload(authed("tenant-a", begin(body.len() as u64)))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (resumed.upload_id.as_str(), resumed.received_bytes),
        (started.upload_id.as_str(), 1000)
    );
    let resent = client
        .upload_artifact_chunk(authed("tenant-a", chunk(&started.upload_id, 0, 1000)))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resent.received_bytes, 1000);
    let gap = client
        .upload_artifact_chunk(authed("tenant-a", chunk(&started.upload_id, 2000, 3000)))
        .await
        .unwrap_err();
    assert_eq!(gap.code(), tonic::Code::FailedPrecondition);
    client
        .upload_artifact_chunk(authed("tenant-a", chunk(&started.upload_id, 1000, 3000)))
        .await
        .unwrap();
    let past_size = client
        .upload_artifact_chunk(authed(
            "tenant-a",
            UploadArtifactChunkRequest {
                upload_id: started.upload_id.clone(),
                offset: 3000,
                data: vec![0; 10],
            },
        ))
        .await
        .unwrap_err();
    assert_eq!(past_size.code(), tonic::Code::ResourceExhausted);
    let done = client
        .complete_artifact_upload(authed(
            "tenant-a",
            CompleteArtifactUploadRequest {
                upload_id: started.upload_id.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(done.complete);
    assert_eq!(read(&mut client, "tenant-a", &id).await.unwrap(), body);
    // Another namespace has not stored it, so it cannot read or name it.
    assert_eq!(
        read(&mut client, "tenant-b", &id).await.unwrap_err().code(),
        tonic::Code::NotFound
    );

    // A version can only name a stored artifact.
    for tenant in ["tenant-a", "tenant-b"] {
        client
            .create_model(authed(
                tenant,
                CreateModelRequest {
                    name: "churn".into(),
                    description: String::new(),
                },
            ))
            .await
            .unwrap();
    }
    let register = |artifact_digest: &str| RegisterVersionRequest {
        model: "churn".into(),
        artifact_digest: artifact_digest.into(),
        framework: "xgboost".into(),
        format: "onnx".into(),
        ..Default::default()
    };
    client
        .register_version(authed("tenant-a", register(&id)))
        .await
        .unwrap();
    let unstored = client
        .register_version(authed("tenant-b", register(&id)))
        .await
        .unwrap_err();
    assert_eq!(unstored.code(), tonic::Code::FailedPrecondition);

    // An upload opened with the wrong size can start over with the right one.
    let other = b"second model".to_vec();
    let opened = client
        .begin_artifact_upload(authed(
            "tenant-a",
            BeginArtifactUploadRequest {
                digest: digest(&other),
                size_bytes: other.len() as u64 + 5,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let corrected = client
        .begin_artifact_upload(authed(
            "tenant-a",
            BeginArtifactUploadRequest {
                digest: digest(&other),
                size_bytes: other.len() as u64,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(corrected.upload_id, opened.upload_id);
    assert_eq!(corrected.size_bytes, other.len() as u64);

    // Bytes that do not match the declared digest are refused at completion.
    let wrong = client
        .begin_artifact_upload(authed(
            "tenant-a",
            BeginArtifactUploadRequest {
                digest: digest(b"declared"),
                size_bytes: 8,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    client
        .upload_artifact_chunk(authed(
            "tenant-a",
            UploadArtifactChunkRequest {
                upload_id: wrong.upload_id.clone(),
                offset: 0,
                data: b"tampered".to_vec(),
            },
        ))
        .await
        .unwrap();
    let mismatch = client
        .complete_artifact_upload(authed(
            "tenant-a",
            CompleteArtifactUploadRequest {
                upload_id: wrong.upload_id,
            },
        ))
        .await
        .unwrap_err();
    assert_eq!(mismatch.code(), tonic::Code::InvalidArgument);

    // A corrupted artifact is refused on read.
    let hex = id.trim_start_matches("sha256:");
    raw.put(
        &StorePath::from(format!("artifacts/sha256/{hex}")),
        PutPayload::from(vec![0u8; body.len()]),
    )
    .await
    .unwrap();
    assert_eq!(
        read(&mut client, "tenant-a", &id).await.unwrap_err().code(),
        tonic::Code::DataLoss
    );

    // Its namespace repairs it by sending the bytes again.
    let reopened = client
        .begin_artifact_upload(authed("tenant-a", begin(body.len() as u64)))
        .await
        .unwrap()
        .into_inner();
    assert_eq!((reopened.complete, reopened.received_bytes), (false, 0));
    client
        .upload_artifact_chunk(authed("tenant-a", chunk(&reopened.upload_id, 0, 3000)))
        .await
        .unwrap();
    client
        .complete_artifact_upload(authed(
            "tenant-a",
            CompleteArtifactUploadRequest {
                upload_id: reopened.upload_id,
            },
        ))
        .await
        .unwrap();
    assert_eq!(read(&mut client, "tenant-a", &id).await.unwrap(), body);
}

#[tokio::test]
async fn artifacts_on_local_disk() {
    let dir = tempfile::tempdir().unwrap();
    let raw = object_store::local::LocalFileSystem::new_with_prefix(dir.path()).unwrap();
    scenarios(Arc::new(raw)).await;
}

#[tokio::test]
async fn artifacts_on_an_s3_compatible_store() {
    let (Ok(endpoint), Ok(bucket)) = (
        std::env::var("HINAGATA_TEST_S3_ENDPOINT"),
        std::env::var("HINAGATA_TEST_S3_BUCKET"),
    ) else {
        assert!(
            std::env::var("HINAGATA_REQUIRE_S3").as_deref() != Ok("1"),
            "HINAGATA_REQUIRE_S3=1 but no S3-compatible store is configured"
        );
        eprintln!("skip: set HINAGATA_TEST_S3_ENDPOINT and HINAGATA_TEST_S3_BUCKET");
        return;
    };
    let store = object_store::aws::AmazonS3Builder::from_env()
        .with_bucket_name(bucket)
        .with_endpoint(&endpoint)
        .with_allow_http(endpoint.starts_with("http://"))
        .build()
        .unwrap();
    // A key space of its own, so concurrent runs never share objects.
    let prefix = format!(
        "test-{}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    scenarios(Arc::new(object_store::prefix::PrefixStore::new(
        store, prefix,
    )))
    .await;
}
