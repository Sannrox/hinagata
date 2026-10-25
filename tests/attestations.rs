//! Promotion attestations (#39): an approved promotion signs a payload a
//! verifier written only from `docs/promotion.md` accepts; a tampered byte
//! or another version's artifact is refused; export includes the
//! attestation in a bundle.

use axum::http::{HeaderMap, StatusCode};
use base64::Engine as _;
use ed25519_dalek::{Signature, Verifier as _, VerifyingKey};
use hinagata::api::proto::hinagata_client::HinagataClient;
use hinagata::api::proto::{
    BeginArtifactUploadRequest, CompleteArtifactUploadRequest, CreateModelRequest,
    GetPromotionAttestationRequest, RegisterVersionRequest, SetVersionStageRequest,
    SignedAttestation, TensorSpec, UploadArtifactChunkRequest,
};
use hinagata::api::{Api, serve};
use hinagata::auth::StaticToken;
use hinagata::promotion::ApprovalHook;
use hinagata::registry::Registry;
use hinagata::signing::Signer;
use sha2::{Digest as _, Sha256};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const TOKEN: &str = "attestation-test-token";
const HOOK_TOKEN: &str = "hook-test-token";
const SEED: &str = "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60";
const V1: &[u8] = b"weights version one";
const V2: &[u8] = b"weights version two!!";

#[derive(Clone, Default)]
struct Hook {
    answer: Arc<Mutex<(u16, serde_json::Value)>>,
}

impl Hook {
    async fn start(&self) -> String {
        let hook = self.clone();
        let router = axum::Router::new().route(
            "/approve",
            axum::routing::post(
                move |headers: HeaderMap, axum::Json(_request): axum::Json<serde_json::Value>| {
                    let hook = hook.clone();
                    async move {
                        let bearer = format!("Bearer {HOOK_TOKEN}");
                        if headers.get("authorization").and_then(|v| v.to_str().ok())
                            != Some(bearer.as_str())
                        {
                            return (StatusCode::UNAUTHORIZED, axum::Json(serde_json::json!({})));
                        }
                        let (status, body) = hook.answer.lock().unwrap().clone();
                        (StatusCode::from_u16(status).unwrap(), axum::Json(body))
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await });
        format!("http://{address}/approve")
    }

    fn allow(&self) {
        *self.answer.lock().unwrap() = (
            200,
            serde_json::json!({
                "decision": "allow",
                "approver": "alice@governance",
                "reference": "CHG-1042",
                "reason": "evaluation reviewed"
            }),
        );
    }
}

struct Harness {
    address: String,
    client: HinagataClient<tonic::transport::Channel>,
}

async fn server(hook: Option<&Hook>, signed: bool) -> Harness {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let mut api = Api::new(
        Arc::new(StaticToken::new(TOKEN, "tenant-a")),
        Arc::new(Registry::open_in_memory().unwrap()),
        Arc::new(hinagata::artifacts::ArtifactStore::new(
            Arc::new(object_store::memory::InMemory::new()),
            1 << 20,
        )),
    );
    if let Some(hook) = hook {
        let url = hook.start().await;
        api = api.with_approval_hook(
            ApprovalHook::new(&url, Some(HOOK_TOKEN.into()), Duration::from_secs(5)).unwrap(),
        );
    }
    if signed {
        api = api.with_signer(Signer::from_seed_hex("registry-1", SEED).unwrap());
    }
    tokio::spawn(serve(api, listener, None, async {
        std::future::pending::<()>().await
    }));
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
    Harness { address, client }
}

fn authed<T>(message: T) -> tonic::Request<T> {
    let mut request = tonic::Request::new(message);
    request
        .metadata_mut()
        .insert("authorization", format!("Bearer {TOKEN}").parse().unwrap());
    request
}

async fn register(client: &mut HinagataClient<tonic::transport::Channel>, bytes: &[u8]) -> u64 {
    let digest = format!("sha256:{:x}", Sha256::digest(bytes));
    let upload = client
        .begin_artifact_upload(authed(BeginArtifactUploadRequest {
            digest: digest.clone(),
            size_bytes: bytes.len() as u64,
        }))
        .await
        .unwrap()
        .into_inner();
    client
        .upload_artifact_chunk(authed(UploadArtifactChunkRequest {
            upload_id: upload.upload_id.clone(),
            offset: 0,
            data: bytes.to_vec(),
        }))
        .await
        .unwrap();
    client
        .complete_artifact_upload(authed(CompleteArtifactUploadRequest {
            upload_id: upload.upload_id,
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
        .unwrap()
        .into_inner()
        .version
}

async fn promote(client: &mut HinagataClient<tonic::transport::Channel>, version: u64) {
    let promoted = client
        .set_version_stage(authed(SetVersionStageRequest {
            model: "churn".into(),
            version,
            stage: "production".into(),
            expected_stage: String::new(),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(promoted.stage, "production");
}

fn public_key() -> [u8; 32] {
    let hex = Signer::from_seed_hex("registry-1", SEED)
        .unwrap()
        .public_key_hex();
    std::array::from_fn(|index| u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).unwrap())
}

fn keys_json() -> String {
    format!(
        r#"{{"keys":[{{"key_id":"registry-1","public_key":"{}"}}]}}"#,
        Signer::from_seed_hex("registry-1", SEED)
            .unwrap()
            .public_key_hex()
    )
}

/// A verifier built only from `docs/promotion.md`.
fn independent_verify(
    public_key: &[u8; 32],
    signed: &SignedAttestation,
    artifact: &[u8],
) -> Result<serde_json::Value, &'static str> {
    if signed.format != "hinagata.signed-attestation/v1" {
        return Err("format");
    }
    let key = VerifyingKey::from_bytes(public_key).map_err(|_| "key")?;
    let signature = base64::engine::general_purpose::STANDARD
        .decode(&signed.signature)
        .map_err(|_| "base64")?;
    let signature = Signature::from_slice(&signature).map_err(|_| "signature")?;
    key.verify(signed.attestation.as_bytes(), &signature)
        .map_err(|_| "forged")?;
    let attestation: serde_json::Value =
        serde_json::from_str(&signed.attestation).map_err(|_| "attestation")?;
    if attestation["format"] != "hinagata.promotion-attestation/v1" {
        return Err("payload");
    }
    if attestation["artifact_digest"] != format!("sha256:{:x}", Sha256::digest(artifact)) {
        return Err("artifact");
    }
    Ok(attestation)
}

async fn http_get(address: &str, path: &str) -> (u16, String) {
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let request = format!(
        "GET {path} HTTP/1.1\r\nhost: hinagata\r\nauthorization: Bearer {TOKEN}\r\nconnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    let status = response[9..12].parse().unwrap();
    let body = response.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    (status, body)
}

fn cli(args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_hinagata"))
        .args(args)
        .output()
        .unwrap()
}

#[tokio::test]
async fn a_signed_attestation_verifies_offline_and_refuses_tampering() {
    let hook = Hook::default();
    hook.allow();
    let mut harness = server(Some(&hook), true).await;
    let version = register(&mut harness.client, V1).await;
    promote(&mut harness.client, version).await;

    let signed = harness
        .client
        .get_promotion_attestation(authed(GetPromotionAttestationRequest {
            model: "churn".into(),
            version,
            stage: String::new(),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(signed.key_id, "registry-1");
    let attestation = independent_verify(&public_key(), &signed, V1).unwrap();
    assert_eq!(attestation["namespace"], "tenant-a");
    assert_eq!(attestation["model"], "churn");
    assert_eq!(attestation["version"], version);
    assert_eq!(attestation["stage"], "production");
    assert_eq!(attestation["approver"], "alice@governance");
    assert_eq!(attestation["reference"], "CHG-1042");
    assert_eq!(
        independent_verify(&public_key(), &signed, V2),
        Err("artifact")
    );
    let forged = SignedAttestation {
        attestation: signed.attestation.replace("CHG-1042", "CHG-0000"),
        ..signed.clone()
    };
    assert_eq!(
        independent_verify(&public_key(), &forged, V1),
        Err("forged")
    );

    let (status, body) = http_get(
        &harness.address,
        "/v1/models/churn/versions/1/promotion-attestation",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["attestation"], signed.attestation);
    assert_eq!(json["signature"], signed.signature);

    let dir = tempfile::tempdir().unwrap();
    let path = |name: &str| dir.path().join(name).to_str().unwrap().to_string();
    std::fs::write(path("attestation.json"), &body).unwrap();
    std::fs::write(path("model.onnx"), V1).unwrap();
    std::fs::write(path("other.onnx"), V2).unwrap();
    std::fs::write(path("keys.json"), keys_json()).unwrap();
    let verify = |artifact: &str| {
        cli(&[
            "verify",
            "--keys",
            &path("keys.json"),
            "--attestation",
            &path("attestation.json"),
            "--artifact",
            &path(artifact),
        ])
    };
    let accepted = verify("model.onnx");
    assert!(accepted.status.success(), "{accepted:?}");
    let refused = verify("other.onnx");
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("does not match"));
}

#[tokio::test]
async fn an_attestation_for_one_version_never_covers_another() {
    let hook = Hook::default();
    hook.allow();
    let mut harness = server(Some(&hook), true).await;
    let first = register(&mut harness.client, V1).await;
    let second = register(&mut harness.client, V2).await;
    promote(&mut harness.client, first).await;
    promote(&mut harness.client, second).await;

    let first_signed = harness
        .client
        .get_promotion_attestation(authed(GetPromotionAttestationRequest {
            model: "churn".into(),
            version: first,
            stage: String::new(),
        }))
        .await
        .unwrap()
        .into_inner();
    let second_signed = harness
        .client
        .get_promotion_attestation(authed(GetPromotionAttestationRequest {
            model: "churn".into(),
            version: second,
            stage: String::new(),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        independent_verify(&public_key(), &first_signed, V2),
        Err("artifact")
    );
    assert_eq!(
        independent_verify(&public_key(), &second_signed, V1),
        Err("artifact")
    );
    let first_payload: serde_json::Value = serde_json::from_str(&first_signed.attestation).unwrap();
    let second_payload: serde_json::Value =
        serde_json::from_str(&second_signed.attestation).unwrap();
    assert_ne!(
        first_payload["artifact_digest"],
        second_payload["artifact_digest"]
    );
    assert_eq!(first_payload["version"], first);
    assert_eq!(second_payload["version"], second);
}

#[tokio::test]
async fn without_an_approval_or_signing_key_attestations_are_refused() {
    let hook = Hook::default();
    hook.allow();
    let mut unsigned = server(Some(&hook), false).await;
    let version = register(&mut unsigned.client, V1).await;
    promote(&mut unsigned.client, version).await;
    let refused = unsigned
        .client
        .get_promotion_attestation(authed(GetPromotionAttestationRequest {
            model: "churn".into(),
            version,
            stage: String::new(),
        }))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), tonic::Code::FailedPrecondition);

    let mut signed = server(None, true).await;
    let version = register(&mut signed.client, V1).await;
    let refused = signed
        .client
        .get_promotion_attestation(authed(GetPromotionAttestationRequest {
            model: "churn".into(),
            version,
            stage: String::new(),
        }))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), tonic::Code::FailedPrecondition);
    assert!(refused.message().contains("no approved promotion"));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_exported_bundle_attestation_verifies_and_a_tampered_one_is_refused() {
    let hook = Hook::default();
    hook.allow();
    let mut harness = server(Some(&hook), true).await;
    register(&mut harness.client, V1).await;
    register(&mut harness.client, V2).await;
    promote(&mut harness.client, 1).await;
    promote(&mut harness.client, 2).await;

    let dir = tempfile::tempdir().unwrap();
    let path = |name: &str| dir.path().join(name).to_str().unwrap().to_string();
    std::fs::write(path("token"), format!("{TOKEN}\n")).unwrap();
    std::fs::write(path("keys.json"), keys_json()).unwrap();
    let exported = cli(&[
        "export",
        "--server",
        &format!("http://{}", harness.address),
        "--token-file",
        &path("token"),
        "--model",
        "churn",
        "--version",
        "1",
        "--version",
        "2",
        "--out",
        &path("bundle"),
    ]);
    assert!(exported.status.success(), "{exported:?}");

    let attestation_path = dir.path().join("bundle/attestations/churn-1.json");
    let signed: SignedAttestation =
        serde_json::from_slice(&std::fs::read(&attestation_path).unwrap()).unwrap();
    independent_verify(&public_key(), &signed, V1).unwrap();

    let mut tampered = signed.clone();
    tampered.attestation = tampered.attestation.replace("CHG-1042", "CHG-0000");
    let bad = dir.path().join("bad");
    copy_dir(&dir.path().join("bundle"), &bad);
    std::fs::write(
        bad.join("attestations/churn-1.json"),
        serde_json::to_vec_pretty(&tampered).unwrap(),
    )
    .unwrap();
    let keys = hinagata::signing::TrustedKeys::from_json(&keys_json()).unwrap();
    let refused = hinagata::bundle::verify(&keys, &bad)
        .err()
        .expect("tampered");
    assert!(refused.contains("does not match"), "{refused}");

    std::fs::copy(
        dir.path().join("bundle/attestations/churn-1.json"),
        dir.path().join("bundle/attestations/churn-2.json"),
    )
    .unwrap();
    let swapped = hinagata::bundle::verify(&keys, &dir.path().join("bundle"))
        .err()
        .expect("swapped");
    assert!(swapped.contains("attestation is for"), "{swapped}");
}

#[test]
fn bundle_verify_refuses_an_attestation_from_another_namespace() {
    use hinagata::registry::{Lineage, Signature, Stage, Version};
    use hinagata::signing::TrustedKeys;
    use std::collections::BTreeMap;

    let signer = Signer::from_seed_hex("registry-1", SEED).unwrap();
    let version = Version {
        model: "churn".into(),
        version: 1,
        artifact_digest: format!("sha256:{:x}", Sha256::digest(V1)),
        framework: "xgboost".into(),
        format: "onnx".into(),
        signature: Signature::default(),
        metrics: BTreeMap::new(),
        tags: BTreeMap::new(),
        lineage: Lineage::default(),
        binding: None,
        stage: Stage::Production,
        created_at_ms: 1,
        created_by: "svc".into(),
    };
    let (envelope, metadata) = signer.sign("tenant-a", &version).unwrap();
    let approval = hinagata::promotion::Approval {
        stage: Stage::Production,
        approver: "alice@governance".into(),
        reference: "CHG-1042".into(),
    };
    let foreign = hinagata::attestation::sign(&signer, "tenant-b", &version, &approval).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let bundle = dir.path().join("bundle");
    std::fs::create_dir_all(bundle.join("manifests")).unwrap();
    std::fs::create_dir_all(bundle.join("attestations")).unwrap();
    std::fs::create_dir_all(bundle.join("artifacts")).unwrap();
    std::fs::write(
        bundle.join("bundle.json"),
        r#"{"format":"hinagata.bundle/v1"}"#,
    )
    .unwrap();
    let hex = version.artifact_digest.trim_start_matches("sha256:");
    std::fs::write(bundle.join("artifacts").join(hex), V1).unwrap();
    std::fs::write(
        bundle.join("manifests/churn-1.json"),
        serde_json::to_vec_pretty(&hinagata::api::proto::VersionManifest {
            format: envelope.format,
            key_id: envelope.key_id,
            manifest: envelope.manifest,
            signature: envelope.signature,
            metadata,
        })
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        bundle.join("attestations/churn-1.json"),
        serde_json::to_vec_pretty(&SignedAttestation {
            format: foreign.format,
            key_id: foreign.key_id,
            attestation: foreign.attestation,
            signature: foreign.signature,
        })
        .unwrap(),
    )
    .unwrap();
    let keys = TrustedKeys::from_json(&keys_json()).unwrap();
    let refused = hinagata::bundle::verify(&keys, &bundle)
        .err()
        .expect("foreign namespace");
    assert!(
        refused.contains("tenant-b") && refused.contains("tenant-a"),
        "{refused}"
    );
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}
