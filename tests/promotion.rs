//! Promotion (#5): production promotion without an approval is refused, a
//! deny or a failing hook keeps the version where it is, gates refuse before
//! the hook is asked, and an approved promotion records the approver and
//! the decision reference.

use axum::http::{HeaderMap, StatusCode};
use hinagata::api::proto::hinagata_client::HinagataClient;
use hinagata::api::proto::{
    BeginArtifactUploadRequest, CompleteArtifactUploadRequest, CreateModelRequest,
    ListAuditEventsRequest, PromotionGates, PutPromotionGatesRequest, RegisterVersionRequest,
    SetVersionStageRequest, TensorSpec, UploadArtifactChunkRequest,
};
use hinagata::api::{Api, serve};
use hinagata::auth::StaticToken;
use hinagata::promotion::ApprovalHook;
use hinagata::registry::Registry;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const TOKEN: &str = "promotion-test-token";
const HOOK_TOKEN: &str = "hook-test-token";
const SEED: &str = "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60";

/// A stand-in approval hook: records each request and answers with the
/// configured status and body, after checking the registry's token.
#[derive(Clone, Default)]
struct Hook {
    requests: Arc<Mutex<Vec<serde_json::Value>>>,
    answer: Arc<Mutex<(u16, serde_json::Value)>>,
}

impl Hook {
    async fn start(&self) -> String {
        let hook = self.clone();
        let router = axum::Router::new().route(
            "/approve",
            axum::routing::post(
                move |headers: HeaderMap, axum::Json(request): axum::Json<serde_json::Value>| {
                    let hook = hook.clone();
                    async move {
                        let bearer = format!("Bearer {HOOK_TOKEN}");
                        if headers.get("authorization").and_then(|v| v.to_str().ok())
                            != Some(bearer.as_str())
                        {
                            return (StatusCode::UNAUTHORIZED, axum::Json(serde_json::json!({})));
                        }
                        hook.requests.lock().unwrap().push(request);
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

    fn answer(&self, status: u16, body: serde_json::Value) {
        *self.answer.lock().unwrap() = (status, body);
    }

    fn asked(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

async fn server(hook: Option<&Hook>, signed: bool) -> HinagataClient<tonic::transport::Channel> {
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
        api =
            api.with_signer(hinagata::signing::Signer::from_seed_hex("registry-1", SEED).unwrap());
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
    client
}

fn authed<T>(message: T) -> tonic::Request<T> {
    let mut request = tonic::Request::new(message);
    request
        .metadata_mut()
        .insert("authorization", format!("Bearer {TOKEN}").parse().unwrap());
    request
}

fn input(name: &str) -> TensorSpec {
    TensorSpec {
        name: name.into(),
        dtype: "float32".into(),
        shape: vec![-1, 4],
        nullable: false,
    }
}

/// Store `bytes` and register them as the next version of `churn`.
async fn register(
    client: &mut HinagataClient<tonic::transport::Channel>,
    bytes: &[u8],
    inputs: Vec<TensorSpec>,
) -> u64 {
    let digest = {
        use sha2::Digest as _;
        format!("sha256:{:x}", sha2::Sha256::digest(bytes))
    };
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
                inputs,
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

async fn promote(
    client: &mut HinagataClient<tonic::transport::Channel>,
    version: u64,
    stage: &str,
) -> Result<hinagata::api::proto::Version, tonic::Status> {
    client
        .set_version_stage(authed(SetVersionStageRequest {
            model: "churn".into(),
            version,
            stage: stage.into(),
            expected_stage: String::new(),
        }))
        .await
        .map(tonic::Response::into_inner)
}

async fn events(client: &mut HinagataClient<tonic::transport::Channel>) -> Vec<(String, String)> {
    client
        .list_audit_events(authed(ListAuditEventsRequest {
            model: "churn".into(),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner()
        .events
        .into_iter()
        .map(|event| (event.action, event.after))
        .collect()
}

#[tokio::test]
async fn production_without_an_allow_is_refused() {
    // No hook: production is refused, other changes apply.
    let mut client = server(None, false).await;
    let version = register(&mut client, b"weights v1", vec![input("features")]).await;
    let refused = promote(&mut client, version, "production")
        .await
        .unwrap_err();
    assert_eq!(refused.code(), tonic::Code::FailedPrecondition);
    assert!(refused.message().contains("approval hook"), "{refused:?}");
    assert_eq!(
        promote(&mut client, version, "staging")
            .await
            .unwrap()
            .stage,
        "staging"
    );

    // A deny, a failing hook and an allow without a reference all leave the
    // version where it is.
    let hook = Hook::default();
    let mut client = server(Some(&hook), false).await;
    let version = register(&mut client, b"weights v1", vec![input("features")]).await;
    for (status, body) in [
        (
            200,
            serde_json::json!({"decision": "deny", "reason": "no change ticket"}),
        ),
        (500, serde_json::json!({})),
        (
            200,
            serde_json::json!({"decision": "allow", "approver": "alice"}),
        ),
        (200, serde_json::json!({"decision": "maybe"})),
    ] {
        hook.answer(status, body);
        let refused = promote(&mut client, version, "production")
            .await
            .unwrap_err();
        assert_eq!(
            refused.code(),
            tonic::Code::FailedPrecondition,
            "{refused:?}"
        );
    }
    assert_eq!(hook.asked(), 4);
    // Asking again about the same change sends the same request id.
    let ids: Vec<_> = hook
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|request| request["request_id"].clone())
        .collect();
    assert!(ids.iter().all(|id| *id == ids[0]), "{ids:?}");
    let audit = events(&mut client).await;
    let denied: Vec<_> = audit
        .iter()
        .filter(|(action, _)| action == "version.stage_denied")
        .collect();
    assert_eq!(denied.len(), 4);
    assert!(denied[0].1.contains("no change ticket"));
    assert!(
        audit
            .iter()
            .all(|(action, _)| action != "version.stage_changed")
    );

    // With a hook, every stage change is asked about.
    hook.answer(
        200,
        serde_json::json!({"decision": "deny", "reason": "frozen"}),
    );
    let refused = promote(&mut client, version, "staging").await.unwrap_err();
    assert!(refused.message().contains("frozen"), "{refused:?}");
}

#[tokio::test]
async fn an_approved_promotion_records_the_approver_and_the_decision_reference() {
    let hook = Hook::default();
    let mut client = server(Some(&hook), true).await;
    let version = register(&mut client, b"weights v1", vec![input("features")]).await;
    client
        .put_promotion_gates(authed(PutPromotionGatesRequest {
            model: "churn".into(),
            gates: Some(PromotionGates {
                min_metrics: [("auc".to_string(), 0.85)].into(),
                require_compatible_signature: true,
                require_signed_manifest: true,
            }),
        }))
        .await
        .unwrap();
    hook.answer(
        200,
        serde_json::json!({
            "decision": "allow",
            "approver": "alice@governance",
            "reference": "CHG-1042",
            "reason": "evaluation reviewed"
        }),
    );
    let promoted = promote(&mut client, version, "production").await.unwrap();
    assert_eq!(promoted.stage, "production");

    let request = hook.requests.lock().unwrap()[0].clone();
    assert_eq!(request["format"], "hinagata.approval-request/v1");
    assert_eq!(request["namespace"], "tenant-a");
    assert_eq!(request["from_stage"], "candidate");
    assert_eq!(request["to_stage"], "production");
    assert_eq!(request["gates"].as_array().unwrap().len(), 3);
    // The signed manifest verifies with the registry's public key.
    let keys = hinagata::signing::TrustedKeys::from_json(&format!(
        r#"{{"keys":[{{"key_id":"registry-1","public_key":"{}"}}]}}"#,
        hinagata::signing::Signer::from_seed_hex("registry-1", SEED)
            .unwrap()
            .public_key_hex()
    ))
    .unwrap();
    let envelope: hinagata::signing::Envelope =
        serde_json::from_value(request["manifest"].clone()).unwrap();
    assert_eq!(keys.verify(&envelope).unwrap().version, version);

    let audit = events(&mut client).await;
    let (_, approved) = audit
        .iter()
        .find(|(action, _)| action == "version.stage_approved")
        .unwrap();
    let approved: serde_json::Value = serde_json::from_str(approved).unwrap();
    assert_eq!(approved["approver"], "alice@governance");
    assert_eq!(approved["reference"], "CHG-1042");
    assert_eq!(approved["request_id"], request["request_id"]);
    assert!(
        audit
            .iter()
            .any(|(action, _)| action == "version.stage_changed")
    );

    // Promoting again from the same stage asks nothing.
    promote(&mut client, version, "production").await.unwrap();
    assert_eq!(hook.asked(), 1);
}

#[tokio::test]
async fn failed_gates_refuse_before_the_hook_is_asked() {
    let hook = Hook::default();
    hook.answer(
        200,
        serde_json::json!({"decision": "allow", "approver": "alice", "reference": "CHG-1"}),
    );
    let mut client = server(Some(&hook), false).await;
    let first = register(&mut client, b"weights v1", vec![input("features")]).await;
    promote(&mut client, first, "production").await.unwrap();
    let second = register(
        &mut client,
        b"weights v2",
        vec![input("features"), input("extra")],
    )
    .await;

    for (gates, failure) in [
        (
            PromotionGates {
                min_metrics: [("auc".to_string(), 0.9)].into(),
                ..Default::default()
            },
            "metric:auc",
        ),
        (
            PromotionGates {
                min_metrics: [("recall".to_string(), 0.5)].into(),
                ..Default::default()
            },
            "metric:recall",
        ),
        (
            PromotionGates {
                require_compatible_signature: true,
                ..Default::default()
            },
            "against version 1",
        ),
        (
            PromotionGates {
                require_signed_manifest: true,
                ..Default::default()
            },
            "no signing key",
        ),
    ] {
        client
            .put_promotion_gates(authed(PutPromotionGatesRequest {
                model: "churn".into(),
                gates: Some(gates),
            }))
            .await
            .unwrap();
        let refused = promote(&mut client, second, "production")
            .await
            .unwrap_err();
        assert_eq!(refused.code(), tonic::Code::FailedPrecondition);
        assert!(refused.message().contains(failure), "{refused:?}");
    }
    assert_eq!(hook.asked(), 1);
}

#[test]
fn the_hook_must_be_https_or_loopback() {
    let timeout = Duration::from_secs(1);
    assert!(ApprovalHook::new("http://approvals.example.com/hook", None, timeout).is_err());
    assert!(ApprovalHook::new("https://approvals.example.com/hook", None, timeout).is_ok());
    assert!(ApprovalHook::new("http://127.0.0.1:9/hook", None, timeout).is_ok());
    let env = |pairs: &'static [(&'static str, &'static str)]| {
        move |name: &str| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        }
    };
    assert!(hinagata::config::approval_hook(env(&[])).unwrap().is_none());
    assert!(
        hinagata::config::approval_hook(env(&[("HINAGATA_APPROVAL_TIMEOUT_MS", "5")])).is_err()
    );
    assert!(
        hinagata::config::approval_hook(env(&[
            (
                "HINAGATA_APPROVAL_URL",
                "https://approvals.example.com/hook"
            ),
            ("HINAGATA_APPROVAL_TIMEOUT_MS", "0"),
        ]))
        .is_err()
    );
}
