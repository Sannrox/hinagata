//! Signed version manifests (#12): a registry with a signing key returns a
//! manifest that a verifier written only from `docs/signatures.md` accepts,
//! and that fails once the artifact, metadata or manifest changes.

use base64::Engine as _;
use ed25519_dalek::{Signature, Verifier as _, VerifyingKey};
use hinagata::api::proto::hinagata_client::HinagataClient;
use hinagata::api::proto::{
    BeginArtifactUploadRequest, CompleteArtifactUploadRequest, CreateModelRequest,
    GetVersionManifestRequest, RegisterVersionRequest, TensorSpec, UploadArtifactChunkRequest,
    VersionManifest,
};
use hinagata::api::{Api, serve};
use hinagata::auth::Jwt;
use hinagata::registry::Registry;
use hinagata::signing::Signer;
use jsonwebtoken::{EncodingKey, Header, encode};
use sha2::{Digest as _, Sha256};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const SECRET: &[u8] = b"test-only-hmac-secret-0123456789";
const SEED: &str = "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60";
const MODEL: &[u8] = b"lightgbm churn model";

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

async fn server(signer: Option<Signer>) -> String {
    let k = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(SECRET);
    let jwks = format!(r#"{{"keys":[{{"kty":"oct","kid":"k1","alg":"HS256","k":"{k}"}}]}}"#);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let mut api = Api::new(
        Arc::new(Jwt::new(&jwks, "hinagata", None, "tenant").unwrap()),
        Arc::new(Registry::open_in_memory().unwrap()),
        Arc::new(hinagata::artifacts::ArtifactStore::new(
            Arc::new(object_store::memory::InMemory::new()),
            1 << 20,
        )),
    );
    if let Some(signer) = signer {
        api = api.with_signer(signer);
    }
    tokio::spawn(serve(api, listener, None, async {
        std::future::pending::<()>().await
    }));
    address
}

fn authed<T>(message: T) -> tonic::Request<T> {
    let mut request = tonic::Request::new(message);
    request.metadata_mut().insert(
        "authorization",
        format!("Bearer {}", token("tenant-a")).parse().unwrap(),
    );
    request
}

/// Create `churn`, store `MODEL` and register it as version 1.
async fn register(address: &str) -> HinagataClient<tonic::transport::Channel> {
    let mut client = HinagataClient::connect(format!("http://{address}"))
        .await
        .unwrap();
    client
        .create_model(authed(CreateModelRequest {
            name: "churn".into(),
            description: String::new(),
        }))
        .await
        .unwrap();
    let digest = format!("sha256:{:x}", Sha256::digest(MODEL));
    let begun = client
        .begin_artifact_upload(authed(BeginArtifactUploadRequest {
            digest: digest.clone(),
            size_bytes: MODEL.len() as u64,
        }))
        .await
        .unwrap()
        .into_inner();
    client
        .upload_artifact_chunk(authed(UploadArtifactChunkRequest {
            upload_id: begun.upload_id.clone(),
            offset: 0,
            data: MODEL.to_vec(),
        }))
        .await
        .unwrap();
    client
        .complete_artifact_upload(authed(CompleteArtifactUploadRequest {
            upload_id: begun.upload_id,
        }))
        .await
        .unwrap();
    client
        .register_version(authed(RegisterVersionRequest {
            model: "churn".into(),
            artifact_digest: digest,
            framework: "lightgbm".into(),
            format: "onnx".into(),
            signature: Some(hinagata::api::proto::Signature {
                inputs: vec![TensorSpec {
                    name: "features".into(),
                    dtype: "float32".into(),
                    shape: vec![-1, 4],
                    nullable: false,
                }],
                outputs: vec![],
            }),
            metrics: [("auc".to_string(), 0.88)].into(),
            ..Default::default()
        }))
        .await
        .unwrap();
    client
}

async fn http_get(address: &str, path: &str) -> (u16, String) {
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let request = format!(
        "GET {path} HTTP/1.1\r\nhost: hinagata\r\nauthorization: Bearer {}\r\nconnection: close\r\n\r\n",
        token("tenant-a")
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    let status = response[9..12].parse().unwrap();
    let body = response.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    (status, body)
}

/// A verifier built only from `docs/signatures.md`.
fn independent_verify(
    public_key: &[u8; 32],
    envelope: &VersionManifest,
    artifact: &[u8],
) -> Result<serde_json::Value, &'static str> {
    if envelope.format != "hinagata.signature/v1" {
        return Err("format");
    }
    let key = VerifyingKey::from_bytes(public_key).map_err(|_| "key")?;
    let signature = base64::engine::general_purpose::STANDARD
        .decode(&envelope.signature)
        .map_err(|_| "base64")?;
    let signature = Signature::from_slice(&signature).map_err(|_| "signature")?;
    key.verify(envelope.manifest.as_bytes(), &signature)
        .map_err(|_| "forged")?;
    let manifest: serde_json::Value =
        serde_json::from_str(&envelope.manifest).map_err(|_| "manifest")?;
    let digest = |bytes: &[u8]| format!("sha256:{:x}", Sha256::digest(bytes));
    if manifest["format"] != "hinagata.version-manifest/v1" {
        return Err("manifest format");
    }
    if manifest["artifact_digest"] != digest(artifact) {
        return Err("artifact");
    }
    if manifest["metadata_digest"] != digest(envelope.metadata.as_bytes()) {
        return Err("metadata");
    }
    Ok(manifest)
}

fn public_key() -> [u8; 32] {
    let hex = Signer::from_seed_hex("registry-1", SEED)
        .unwrap()
        .public_key_hex();
    std::array::from_fn(|index| u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).unwrap())
}

#[tokio::test]
async fn a_signed_manifest_verifies_offline_and_tampering_fails() {
    let address = server(Some(Signer::from_seed_hex("registry-1", SEED).unwrap())).await;
    let mut client = register(&address).await;
    let envelope = client
        .get_version_manifest(authed(GetVersionManifestRequest {
            model: "churn".into(),
            version: 1,
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(envelope.key_id, "registry-1");
    let manifest = independent_verify(&public_key(), &envelope, MODEL).unwrap();
    assert_eq!(manifest["namespace"], "tenant-a");
    assert_eq!(manifest["model"], "churn");
    assert_eq!(manifest["version"], 1);
    let metadata: serde_json::Value = serde_json::from_str(&envelope.metadata).unwrap();
    assert_eq!(metadata["metrics"]["auc"], 0.88);

    assert_eq!(
        independent_verify(&public_key(), &envelope, b"other weights"),
        Err("artifact")
    );
    let tampered = VersionManifest {
        metadata: envelope.metadata.replace("0.88", "0.99"),
        ..envelope.clone()
    };
    assert_eq!(
        independent_verify(&public_key(), &tampered, MODEL),
        Err("metadata")
    );
    let forged = VersionManifest {
        manifest: envelope.manifest.replace("tenant-a", "tenant-b"),
        ..envelope.clone()
    };
    assert_eq!(
        independent_verify(&public_key(), &forged, MODEL),
        Err("forged")
    );

    // HTTP returns the same envelope, and `hinagata verify` accepts it.
    let (status, body) = http_get(&address, "/v1/models/churn/versions/1/manifest").await;
    assert_eq!(status, 200, "{body}");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["manifest"], envelope.manifest);
    assert_eq!(json["signature"], envelope.signature);

    let dir = tempfile::tempdir().unwrap();
    let path = |name: &str| dir.path().join(name).to_str().unwrap().to_string();
    std::fs::write(path("envelope.json"), &body).unwrap();
    std::fs::write(path("model.onnx"), MODEL).unwrap();
    std::fs::write(path("other.onnx"), b"other weights").unwrap();
    std::fs::write(
        path("keys.json"),
        format!(
            r#"{{"keys":[{{"key_id":"registry-1","public_key":"{}"}}]}}"#,
            Signer::from_seed_hex("registry-1", SEED)
                .unwrap()
                .public_key_hex()
        ),
    )
    .unwrap();
    let verify = |artifact: &str| {
        std::process::Command::new(env!("CARGO_BIN_EXE_hinagata"))
            .args(["verify", "--keys", &path("keys.json")])
            .args(["--envelope", &path("envelope.json")])
            .args(["--artifact", &path(artifact)])
            .output()
            .unwrap()
    };
    let accepted = verify("model.onnx");
    assert!(accepted.status.success(), "{accepted:?}");
    let refused = verify("other.onnx");
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("does not match"));

    // A flag without its path, or an envelope stripped of its metadata, is
    // refused rather than skipping that check.
    let valueless = std::process::Command::new(env!("CARGO_BIN_EXE_hinagata"))
        .args(["verify", "--keys", &path("keys.json")])
        .args(["--envelope", &path("envelope.json"), "--artifact"])
        .output()
        .unwrap();
    assert!(!valueless.status.success());
    let mut stripped = json.clone();
    stripped.as_object_mut().unwrap().remove("metadata");
    std::fs::write(path("envelope.json"), stripped.to_string()).unwrap();
    let unbundled = verify("model.onnx");
    assert!(!unbundled.status.success());
    assert!(String::from_utf8_lossy(&unbundled.stderr).contains("no metadata"));
}

#[tokio::test]
async fn without_a_signing_key_manifests_are_refused() {
    let address = server(None).await;
    let mut client = register(&address).await;
    let refused = client
        .get_version_manifest(authed(GetVersionManifestRequest {
            model: "churn".into(),
            version: 1,
        }))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), tonic::Code::FailedPrecondition);
    let missing = client
        .get_version_manifest(authed(GetVersionManifestRequest {
            model: "churn".into(),
            version: 2,
        }))
        .await
        .unwrap_err();
    assert_eq!(missing.code(), tonic::Code::FailedPrecondition);
}

#[test]
fn the_signing_key_needs_its_id() {
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("signing.key");
    std::fs::write(&key, format!("{SEED}\n")).unwrap();
    let key = key.to_str().unwrap().to_string();
    let env = |pairs: Vec<(&'static str, String)>| {
        move |name: &str| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.clone())
        }
    };
    let signer = hinagata::config::signer(env(vec![
        ("HINAGATA_SIGNING_KEY_FILE", key.clone()),
        ("HINAGATA_SIGNING_KEY_ID", "registry-1".into()),
    ]))
    .unwrap()
    .unwrap();
    assert_eq!(signer.key_id(), "registry-1");
    assert!(hinagata::config::signer(env(vec![])).unwrap().is_none());
    assert!(hinagata::config::signer(env(vec![("HINAGATA_SIGNING_KEY_FILE", key)])).is_err());
    assert!(hinagata::config::signer(env(vec![("HINAGATA_SIGNING_KEY_ID", "k".into())])).is_err());
}
